import AgentAvatar from "./AgentAvatar";
import {
  ActionIcon,
  Button,
  Loader,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { useEffect, useState, type ReactNode } from "react";
import { ArrowLeft, Inbox, Megaphone, Search, Users } from "lucide-react";
import { save, saved } from "../api";
import { useMessages } from "../hooks";
import type { Snapshot } from "../types";
import StreamingText from "./StreamingText";
import Requests from "./Requests";
import UserMessages from "./UserMessages";
import { useConversationScroll } from "./useConversationScroll";
import "./team-chats.css";

export default function TeamChats({
  data,
  leadId,
  forYou,
  focusRequestId,
  focusItemId,
  focusRoomId,
  refresh,
  notify,
}: {
  data: Snapshot;
  leadId?: string;
  forYou?: ReactNode;
  focusRequestId?: string;
  focusItemId?: string;
  focusRoomId?: string;
  refresh: () => Promise<void>;
  notify: (message: string) => void;
}) {
  const scope = `${data.stateDir}:${leadId}`;
  const [selection, setSelection] = useState({
    scope,
    id: "you",
    detail: false,
  });
  const selected = selection.scope === scope ? selection.id : "you";
  const [query, setQuery] = useState("");
  const [limit, setLimit] = useState(60);
  useEffect(() => {
    setQuery("");
    setLimit(60);
  }, [scope]);
  const seenKey = `codex-chat-seen:${data.stateDir}`;
  const [seen, setSeen] = useState<Record<string, number>>(() =>
    saved(seenKey, {}),
  );
  const members = new Set(
    data.threads
      .filter(
        (agent) => leadId && (agent.id === leadId || agent.rootId === leadId),
      )
      .map((agent) => agent.id),
  );
  const rooms = data.runtime.rooms.filter(
    (room) =>
      leadId &&
      (room.kind === "broadcast"
        ? room.rootId === leadId
        : room.members.length > 0 &&
          (room.members.every((id) => members.has(id)) ||
            (!!room.reviewTargets?.length &&
              room.members.some((id) => members.has(id))))),
  );
  const name = (room: (typeof rooms)[number]) => {
    if (room.kind === "broadcast") return "Team broadcast";
    const peers = room.members.filter((id) => id !== leadId);
    return !room.reviewTargets?.length && peers.length === 1
      ? data.threads.find((agent) => agent.id === peers[0])?.name || room.name
      : room.name;
  };
  const room = rooms.find((room) => room.id === selected);
  const showYou = selected === "you" || !room;
  const choose = (id: string) => setSelection({ scope, id, detail: true });
  useEffect(() => {
    if (focusRoomId) choose(focusRoomId);
    else if (focusItemId || focusRequestId) choose("you");
  }, [scope, focusRoomId, focusItemId, focusRequestId]);
  useEffect(() => {
    setSeen(saved(seenKey, {}));
  }, [seenKey]);
  const attention = data.runtime.requests.filter(
    (request) => request.status === "pending" || !request.status,
  );
  const latestPersonal = [...data.runtime.complaints].sort(
    (a, b) => (b.created || 0) - (a.created || 0),
  )[0];
  const personalPreview =
    attention[0]?.params?.questions?.[0]?.question ||
    attention[0]?.title ||
    latestPersonal?.title ||
    "Questions, messages and replies";
  const icon = (item?: (typeof rooms)[number]) =>
    !item ? (
      <span className="team-conversation-icon personal">
        <Inbox size={21} />
      </span>
    ) : item.kind === "broadcast" ? (
      <span className="team-conversation-icon">
        <Megaphone size={21} />
      </span>
    ) : item.members.length > 2 || !item.members.includes(leadId || "") ? (
      <span className="team-conversation-icon">
        <Users size={21} />
      </span>
    ) : (
      <AgentAvatar
        id={item.members.find((id) => id !== leadId) || item.id}
        size={42}
      />
    );
  const clock = (at?: number) =>
    at
      ? new Date(at * 1000).toLocaleTimeString([], {
          hour: "2-digit",
          minute: "2-digit",
        })
      : "";
  const filteredRooms = rooms
    .filter((item) =>
      `${name(item)} ${item.lastMessage?.text || ""}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    )
    .sort(
      (a, b) =>
        (b.lastMessage?.created || b.updated) -
        (a.lastMessage?.created || a.updated),
    );
  const { items, loaded, before, older, notice, reload } = useMessages(
    !showYou && room ? room.id : null,
    "room",
    false,
    data.stateDir,
  );
  useEffect(() => {
    if (
      !loaded ||
      !selection.detail ||
      !room?.lastMessage ||
      (seen[room.id] || 0) >= room.lastMessage.seq
    )
      return;
    const next = {
      ...saved<Record<string, number>>(seenKey, {}),
      ...seen,
      [room.id]: room.lastMessage.seq,
    };
    save(seenKey, next);
    setSeen(next);
  }, [room?.id, room?.lastMessage?.seq, seenKey, loaded, selection.detail]);
  const { scroll, content, follow, setFollow, onScroll } =
    useConversationScroll(`${scope}:messages:${selected}`, loaded);
  const events = [
    ...(showYou ? [] : items).map((message) => ({
      id: `message:${message.id}`,
      created: message.created || 0,
      content: (
        <article className="team-message" data-message={message.id}>
          <div className="chat-message-author">
            <AgentAvatar id={message.sender || "agent"} size={24} />
            <strong>{message.senderName || "Agent"}</strong>
          </div>
          <StreamingText text={message.text || ""} agentId={message.sender} />
          <time>
            {message.created
              ? new Date(message.created * 1000).toLocaleString()
              : ""}
          </time>
        </article>
      ),
    })),
    ...(showYou ? data.runtime.requests : [])
      .filter((request) => request.status === "pending" || !request.status)
      .map((request) => ({
        id: `request:${request.id}`,
        created: request.created || request.at || 0,
        content: (
          <Requests
            scope={data.stateDir}
            requests={[request]}
            allRequests={data.runtime.requests}
            agents={data.threads}
            refresh={refresh}
            notify={notify}
          />
        ),
      })),
    ...(showYou ? data.runtime.complaints : []).map((message) => ({
      id: `complaint:${message.id}`,
      created: message.created || 0,
      content: (
        <UserMessages
          data={{
            ...data,
            runtime: { ...data.runtime, complaints: [message] },
          }}
          target={
            message.recipient as
              | "user"
              | "lead"
          }
          hideEmpty
          focusId={focusItemId}
          focusRequestId={focusRequestId}
          refresh={refresh}
          notify={notify}
        />
      ),
    })),
  ].sort((a, b) => a.created - b.created);
  useEffect(() => {
    if ((!showYou && !loaded) || !focusItemId) return;
    const target = content.current?.querySelector(
      `[data-feed-item="request:${CSS.escape(focusItemId)}"], [data-feed-item="complaint:${CSS.escape(focusItemId)}"]`,
    );
    if (target) {
      setFollow(false);
      target.scrollIntoView({ block: "center" });
    }
  }, [loaded, selected, focusItemId, focusRequestId, focusRoomId]);
  return (
    <section
      className={`team-chats messages-chat-list ${selection.scope === scope && selection.detail ? "show-room" : ""}`}
      aria-label="Messages"
    >
      <nav className="team-room-list" aria-label="Message chats">
        <div className="team-room-search">
          <TextInput
            aria-label="Search chats"
            placeholder="Search chats"
            leftSection={<Search size={16} />}
            value={query}
            onChange={(event) => {
              setQuery(event.currentTarget.value);
              setLimit(60);
            }}
          />
        </div>
        <div className="team-room-rows">
          {`For you ${personalPreview}`
            .toLowerCase()
            .includes(query.toLowerCase()) && (
            <UnstyledButton
              className={`team-room-row ${showYou ? "selected" : ""}`}
              aria-pressed={showYou}
              data-room="you"
              onClick={() => choose("you")}
            >
              {icon()}
              <div className="team-room-copy">
                <div className="team-room-title">
                  <strong>For you</strong>
                  <time>{clock(latestPersonal?.created)}</time>
                </div>
                <div className="team-room-preview">
                  <span>{personalPreview}</span>
                  {attention.length > 0 && (
                    <i
                      className="team-room-unread"
                      aria-label="Needs your reply"
                    />
                  )}
                </div>
              </div>
            </UnstyledButton>
          )}
          {filteredRooms.slice(0, limit).map((item) => (
            <UnstyledButton
              key={item.id}
              data-room={item.id}
              className={`team-room-row ${room?.id === item.id ? "selected" : ""}`}
              aria-pressed={room?.id === item.id}
              onClick={() => choose(item.id)}
            >
              {icon(item)}
              <div className="team-room-copy">
                <div className="team-room-title">
                  <strong>{name(item)}</strong>
                  <time>{clock(item.lastMessage?.created)}</time>
                </div>
                <div className="team-room-preview">
                  <span>{item.lastMessage?.text || "No messages yet"}</span>
                  {item.lastMessage &&
                    (seen[item.id] || 0) < item.lastMessage.seq && (
                      <i
                        className="team-room-unread"
                        aria-label="Unread messages"
                      />
                    )}
                </div>
              </div>
            </UnstyledButton>
          ))}
          {filteredRooms.length > limit && (
            <Button
              variant="subtle"
              fullWidth
              onClick={() => setLimit((value) => value + 60)}
            >
              More chats
            </Button>
          )}
        </div>
      </nav>
      <div className="team-room-detail unified-messages">
        <header className="team-room-header">
          <ActionIcon
            className="team-room-back"
            variant="subtle"
            aria-label="Back to chats"
            onClick={() => setSelection({ scope, id: selected, detail: false })}
          >
            <ArrowLeft size={20} />
          </ActionIcon>
          {icon(showYou ? undefined : room)}
          <div>
            <strong>{showYou ? "For you" : name(room!)}</strong>
            <span className="team-room-kind">
              {showYou
                ? "Questions, messages and replies"
                : room?.kind === "broadcast"
                  ? "Team conversation"
                  : "Conversation"}
            </span>
          </div>
        </header>
        <div
          className="unified-message-scroll"
          ref={scroll}
          onScroll={onScroll}
        >
          <div ref={content}>
            {!showYou && before != null && (
              <Button
                variant="subtle"
                size="xs"
                onClick={() => {
                  setFollow(false);
                  void older().catch((error) => notify(String(error)));
                }}
              >
                Earlier messages
              </Button>
            )}
            {!showYou && !loaded && <Loader size="sm" />}
            {!showYou && notice && (
              <p role="alert">
                {notice}{" "}
                <Button variant="subtle" onClick={() => void reload()}>
                  Retry
                </Button>
              </p>
            )}
            {events.map((event) => (
              <div key={event.id} data-feed-item={event.id}>
                {event.content}
              </div>
            ))}
            {showYou && forYou}
            {!showYou && loaded && !events.length && (
              <p className="team-chat-empty">No messages yet.</p>
            )}
          </div>
        </div>
        {!follow && (
          <Button
            className="team-latest"
            size="compact-xs"
            onClick={() => setFollow(true)}
          >
            Latest messages
          </Button>
        )}
      </div>
    </section>
  );
}
