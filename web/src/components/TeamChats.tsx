import AgentAvatar from "./AgentAvatar";
import { Button, Loader } from "@mantine/core";
import { useEffect, type ReactNode } from "react";
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
  const { items, loaded, before, older, notice, reload } = useMessages(
    leadId ? `feed:${leadId}` : null,
    "room",
    false,
    data.stateDir,
  );
  const { scroll, content, follow, setFollow, onScroll } =
    useConversationScroll(`${data.stateDir}:messages:${leadId}`, loaded);
  const events = [
    ...items.map((message) => ({
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
    ...data.runtime.requests
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
    ...data.runtime.complaints.map((message) => ({
      id: `complaint:${message.id}`,
      created: message.created || 0,
      content: (
        <UserMessages
          data={{
            ...data,
            runtime: { ...data.runtime, complaints: [message] },
          }}
          target={
            (message.recipient ||
              (message.author === message.leadId ? "user" : "lead")) as
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
    if (!loaded || !focusItemId) return;
    const target = content.current?.querySelector(
      `[data-feed-item="request:${CSS.escape(focusItemId)}"], [data-feed-item="complaint:${CSS.escape(focusItemId)}"]`,
    );
    if (target) {
      setFollow(false);
      target.scrollIntoView({ block: "center" });
    }
  }, [loaded, focusItemId, focusRequestId, focusRoomId]);
  return (
    <section className="unified-messages" aria-label="Messages">
      <div className="unified-message-scroll" ref={scroll} onScroll={onScroll}>
        <div ref={content}>
          {before != null && (
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
          {!loaded && <Loader size="sm" />}
          {notice && (
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
          {forYou}
          {loaded && !events.length && (
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
    </section>
  );
}
