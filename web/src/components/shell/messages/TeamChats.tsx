import MessageDate from "../../conversation/transcript/MessageDate";
import AgentAvatar from "../../agents/AgentAvatar";
import {
  ActionIcon,
  Button,
  Loader,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { ArrowLeft, Inbox, Megaphone, Search, Users } from "lucide-react";
import { save, saved } from "../../../api";
import { useMessages } from "../../../hooks";
import type { Snapshot } from "../../../types";
import StreamingText from "../../conversation/transcript/StreamingText";
import Requests from "../../questions/Requests";
import UserMessages from "./UserMessages";
import {
  useConversationScroll,
  useFollowState,
} from "../../useConversationScroll";
import "./team-chats.css";

export default function TeamChats({
  data,
  leadId,
  focusRequestId,
  focusItemId,
  focusRoomId,
  refresh,
  notify,
}: {
  data: Snapshot;
  leadId?: string;
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
      (room.kind === "federated"
        ? (room.localMembers || room.members).some((id) => members.has(id))
        : room.kind === "broadcast"
          ? room.rootId === leadId
          : room.members.length > 0 &&
            (room.members.every((id) => members.has(id)) ||
              (!!room.peerTeamId &&
                room.members.some((id) => members.has(id))))),
  );
  const name = (room: (typeof rooms)[number]) => {
    if (room.kind === "federated") return room.peerLabel || room.name;
    if (room.kind === "broadcast") return "Team broadcast";
    const peers = room.members.filter((id) => id !== leadId);
    return peers.length === 1
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
  const latestPersonal = [
    ...data.runtime.complaints.map((item) => ({
      created: item.created,
      text: item.title,
    })),
    ...data.runtime.requests.map((item) => ({
      created: item.createdAt ?? item.created ?? item.at,
      text: item.params?.questions?.[0]?.question || item.title,
    })),
  ].sort((a, b) => (b.created || 0) - (a.created || 0))[0];
  const personalPreview =
    latestPersonal?.text || "Questions, messages and replies";
  const icon = (item?: (typeof rooms)[number]) =>
    !item ? (
      <span className="team-conversation-icon personal">
        <Inbox size={21} />
      </span>
    ) : item.kind === "broadcast" ? (
      <span className="team-conversation-icon">
        <Megaphone size={21} />
      </span>
    ) : item.kind === "federated" ? (
      <span className="team-conversation-icon">
        <Users size={21} />
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
  const {
    items,
    loaded,
    before,
    after,
    older,
    newer,
    setPageAnchor,
    showLatest,
    notice,
    reload,
  } = useMessages(
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
  const { scroll, content, getFollow, subscribeFollow, setFollow, onScroll } =
    useConversationScroll(`${scope}:messages:${selected}`, loaded);
  const follow = useFollowState(subscribeFollow, getFollow);
  const events = [
    ...(showYou ? data.runtime.requests : [])
      .filter((request) => request.status === "pending" || !request.status)
      .map((request) => ({
        id: `request:${request.id}`,
        created: request.createdAt ?? request.created ?? request.at ?? 0,
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
          target={message.recipient as "user" | "lead"}
          hideEmpty
          focusId={focusItemId}
          focusRequestId={focusRequestId}
          refresh={refresh}
          notify={notify}
        />
      ),
    })),
  ].sort((a, b) => a.created - b.created);
  const heights = useRef(new Map<string, number>());
  const [virtualRange, setVirtualRange] = useState({
    start: 0,
    end: 40,
    top: 0,
    bottom: 0,
  });
  useLayoutEffect(() => {
    const root = scroll.current;
    if (!root || showYou) return;
    const update = () => {
      const estimate = (message: (typeof items)[number]) =>
        heights.current.get(message.id) ||
        Math.max(112, Math.ceil((message.text?.length || 0) / 78) * 23 + 82);
      const offsets = Array.from<number>({ length: items.length + 1 });
      offsets[0] = 0;
      for (let index = 0; index < items.length; index++)
        offsets[index + 1] = offsets[index] + estimate(items[index]);
      const top = root.scrollTop;
      const bottom = top + root.clientHeight;
      let start = 0;
      while (start < items.length && offsets[start + 1] < top) start++;
      let end = start;
      while (end < items.length && offsets[end] < bottom) end++;
      start = Math.max(0, start - 8);
      end = Math.min(items.length, Math.max(start + 1, end + 8));
      const next = {
        start,
        end,
        top: offsets[start],
        bottom: offsets.at(-1)! - offsets[end],
      };
      setVirtualRange((old) =>
        old.start === next.start &&
        old.end === next.end &&
        old.top === next.top &&
        old.bottom === next.bottom
          ? old
          : next,
      );
    };
    update();
    root.addEventListener("scroll", update, { passive: true });
    const observer = new ResizeObserver((entries) => {
      let changed = false;
      for (const entry of entries) {
        const node = entry.target as HTMLElement;
        if (node === root) {
          changed = true;
          continue;
        }
        const id = node.dataset.roomRow;
        if (!id) continue;
        const size = Math.ceil(entry.contentRect.height);
        if (size && heights.current.get(id) !== size) {
          heights.current.set(id, size);
          changed = true;
        }
      }
      if (changed) update();
    });
    observer.observe(root);
    root
      .querySelectorAll<HTMLElement>("[data-room-row]")
      .forEach((row) => observer.observe(row));
    return () => {
      observer.disconnect();
      root.removeEventListener("scroll", update);
    };
  }, [items, showYou, loaded, virtualRange.start, virtualRange.end]);
  const handleScroll = () => {
    onScroll();
    if (showYou) return;
    const root = scroll.current;
    if (!root) return;
    const bounds = root.getBoundingClientRect();
    const anchor = Array.from(
      root.querySelectorAll<HTMLElement>("[data-message]"),
    ).find((node) => {
      const rect = node.getBoundingClientRect();
      return rect.bottom > bounds.top && rect.top < bounds.bottom;
    });
    setPageAnchor(anchor?.dataset.message || null);
  };
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
                  {latestPersonal && (
                    <MessageDate at={latestPersonal.created} />
                  )}
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
                  {item.lastMessage && (
                    <MessageDate at={item.lastMessage.created} />
                  )}
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
                  : room?.kind === "federated"
                    ? `Remote room · ${room.peerLabel || "paired server"}`
                    : "Conversation"}
            </span>
          </div>
        </header>
        <div
          className="unified-message-scroll"
          data-room-retained={!showYou ? items.length : undefined}
          ref={scroll}
          onScroll={handleScroll}
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
            {!showYou && after != null && (
              <Button
                variant="subtle"
                size="xs"
                onClick={() => {
                  setFollow(false);
                  void newer().catch((error) => notify(String(error)));
                }}
              >
                Later messages
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
            {showYou ? (
              events.map((event) => (
                <div key={event.id} data-feed-item={event.id}>
                  {event.content}
                </div>
              ))
            ) : (
              <>
                <div aria-hidden="true" style={{ height: virtualRange.top }} />
                {items
                  .slice(virtualRange.start, virtualRange.end)
                  .map((message) => (
                    <div key={message.id} data-room-row={message.id}>
                      <article
                        className="team-message"
                        data-message={message.id}
                      >
                        <div className="chat-message-author">
                          <AgentAvatar
                            id={message.sender || "agent"}
                            size={24}
                          />
                          <strong>{message.senderName || "Agent"}</strong>
                        </div>
                        <StreamingText
                          text={message.text || ""}
                          agentId={message.sender}
                        />
                        <MessageDate at={message.created} />
                      </article>
                    </div>
                  ))}
                <div
                  aria-hidden="true"
                  style={{ height: virtualRange.bottom }}
                />
              </>
            )}
            {(showYou || loaded) &&
              !(showYou ? events.length : items.length) && (
                <p className="team-chat-empty">No messages yet.</p>
              )}
          </div>
        </div>
        {!follow && (
          <Button
            className="team-latest"
            size="compact-xs"
            onClick={() => {
              showLatest();
              setFollow(true);
            }}
          >
            Latest messages
          </Button>
        )}
      </div>
    </section>
  );
}
