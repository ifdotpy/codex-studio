import ChatStatus from "../components/agents/ChatStatus";
import { ProviderMark } from "../components/AccountTiles";
import type { ChatIndicator } from "../components/chat-status/chatStatusModel";
import "./sidebar-status.css";

export type ProjectChat = {
  id: string;
  name: string;
  serverId?: string | null;
  provider?: string | null;
  indicator?: ChatIndicator;
  status?: string;
  unread?: boolean;
  updated?: number | null;
  created?: number | null;
  pinned?: boolean;
  inFlight?: boolean;
  team?: boolean;
};

export function keepCompactChat(chat: ProjectChat, now = Date.now() / 1000) {
  return !!(
    chat.team ||
    chat.pinned ||
    chat.inFlight ||
    chat.unread ||
    ["running", "starting", "approval"].includes(chat.status || "") ||
    chat.indicator?.kind === "unread" ||
    (chat.updated || chat.created || 0) >= now - 86400
  );
}

export function ChatServerLine({
  provider,
  alias,
}: {
  provider?: string;
  alias: string;
}) {
  return (
    <span
      className="chat-server-line"
      aria-label={`${provider === "claude" ? "Claude" : "Codex"}, ${alias}`}
    >
      <ProviderMark provider={provider || "codex"} />
      <span>{alias}</span>
    </span>
  );
}

export default function ProjectChatRows({
  chats,
  selected,
  aliases,
  compact,
  query,
  open,
  setCompact,
}: {
  chats: ProjectChat[];
  selected?: { id: string; serverId: string };
  aliases: Record<string, string>;
  compact: boolean;
  query?: string;
  open: (chat: ProjectChat) => void;
  setCompact: (value: boolean) => void;
}) {
  const isSelected = (chat: ProjectChat) =>
    selected?.id === chat.id && selected.serverId === chat.serverId;
  const visible = chats.filter(
    (chat) => query || !compact || isSelected(chat) || keepCompactChat(chat),
  );
  const hidden = chats.length - visible.length;
  return (
    <>
      {visible.map((chat) => (
        <button
          className="server-chat"
          key={JSON.stringify([chat.serverId, chat.id])}
          data-chat={chat.id}
          aria-current={isSelected(chat) ? "page" : undefined}
          onClick={() => open(chat)}
        >
          <span className="row-copy">
            <strong>{chat.name}</strong>
            <ChatServerLine
              provider={chat.provider || undefined}
              alias={aliases[chat.serverId || "local"] || "MAC"}
            />
          </span>
          <ChatStatus
            status={
              chat.indicator ||
              (chat.inFlight ||
              ["running", "starting", "approval"].includes(chat.status || "")
                ? { kind: "working", label: "Working" }
                : chat.status === "failed"
                  ? { kind: "error", label: "Failed" }
                  : chat.unread
                    ? { kind: "unread", label: "Unread result" }
                    : undefined)
            }
            provider={chat.provider || undefined}
          />
        </button>
      ))}
      {!query && (compact ? hidden > 0 : chats.length > 0) && (
        <button
          className="project-show-more"
          onClick={() => setCompact(!compact)}
        >
          {compact ? `Show more (${hidden})` : "Show less"}
        </button>
      )}
    </>
  );
}
