import ChatStatus from "../components/agents/ChatStatus";
import { ProviderMark } from "../components/AccountTiles";
import type { ChatIndicator } from "../components/chat-status/chatStatusModel";
import { DEFAULT_LOCAL_SERVER_ALIAS } from "./serverAliases";
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

export function compactVisibleChats(
  chats: ProjectChat[],
  compact: boolean,
  threshold: number,
  selected?: { id: string; serverId: string },
) {
  const shouldCompact = compact && chats.length >= threshold;
  return chats.filter(
    (chat) =>
      !shouldCompact ||
      (selected?.id === chat.id && selected.serverId === chat.serverId) ||
      keepCompactChat(chat),
  );
}

export function shouldShowOldChatsControl(
  chatCount: number,
  threshold: number,
) {
  return chatCount >= threshold;
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
  hideOldChatsThreshold,
  query,
  open,
  setCompact,
}: {
  chats: ProjectChat[];
  selected?: { id: string; serverId: string };
  aliases: Record<string, string>;
  compact: boolean;
  hideOldChatsThreshold: number;
  query?: string;
  open: (chat: ProjectChat) => void;
  setCompact: (value: boolean) => void;
}) {
  const isSelected = (chat: ProjectChat) =>
    selected?.id === chat.id && selected.serverId === chat.serverId;
  const shouldCompact = compact && chats.length >= hideOldChatsThreshold;
  const visible = query
    ? chats
    : compactVisibleChats(chats, compact, hideOldChatsThreshold, selected);
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
              alias={
                aliases[chat.serverId || "local"] || DEFAULT_LOCAL_SERVER_ALIAS
              }
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
      {!query &&
        shouldShowOldChatsControl(chats.length, hideOldChatsThreshold) &&
        (shouldCompact ? hidden > 0 : chats.length > 0) && (
          <button
            className="project-show-more"
            onClick={() => setCompact(!compact)}
          >
            {shouldCompact ? `Show old (${hidden})` : "Hide old"}
          </button>
        )}
    </>
  );
}
