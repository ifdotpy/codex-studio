import type { ReactNode } from "react";
import type { ChatIndicator } from "../chat-status/chatStatusModel";
import "./conversation-chrome.css";

export type RecentProjectChat = {
  id: string;
  title: string;
  indicator?: ChatIndicator;
};
export type ConversationStartProps = {
  projectName: string;
  location: string;
  execution?: ReactNode;
  recentChats?: RecentProjectChat[];
  onSelect?: (id: string) => void;
};

export default function ConversationStart({
  projectName,
  location,
  execution,
  recentChats = [],
  onSelect,
}: ConversationStartProps) {
  return (
    <section className="conversation-start" aria-label="Start a conversation">
      <header className="conversation-start-heading">
        <h2>{projectName}</h2>
        <p>Describe the work to start a conversation.</p>
      </header>
      <dl className="conversation-start-location">
        <dt>Runs in</dt>
        <dd>{location}</dd>
      </dl>
      {execution && (
        <div className="conversation-start-execution">{execution}</div>
      )}
      {!!recentChats.length && onSelect && (
        <nav className="conversation-recent" aria-label="Recent project chats">
          <h3>Recent in this project</h3>
          {recentChats.map((chat) => (
            <button
              key={chat.id}
              type="button"
              className="conversation-recent-strip"
              data-studio-state={chat.indicator?.kind || "none"}
              onClick={() => onSelect(chat.id)}
              title={chat.title}
            >
              <span className="conversation-recent-title">{chat.title}</span>
              <span className="conversation-recent-status">
                {chat.indicator?.label || "Ready"}
              </span>
            </button>
          ))}
        </nav>
      )}
    </section>
  );
}
