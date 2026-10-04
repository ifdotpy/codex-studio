import { Button, Select, Textarea } from "@mantine/core";
import { useRef, useState } from "react";
import { ApiError, errorText, post, save, saved, type PostBody } from "../api";
import { useMessages } from "../hooks";
import type { Room, Snapshot } from "../types";
import AgentAvatar from "./agents/AgentAvatar";
import MessageDate from "./conversation/transcript/MessageDate";
import Requests from "./questions/Requests";
import StreamingText from "./conversation/transcript/StreamingText";
import { useConversationScroll } from "./useConversationScroll";
import {
  usePromptDraft,
  type DraftReader,
  type DraftSubscription,
} from "./prompt-composer/PromptComposer";
import "./radio-chat.css";

// Keep the exact command across navigation and reloads until the server confirms it.
type PeerTeamRequest = PostBody<"/api/peer-teams">;
type RadioRequest = Extract<
  PeerTeamRequest,
  { action: "radio"; radio_action: "send" | "pass" | "stop" }
>;
type RadioActionInput =
  | { action: "pass"; target: string }
  | { action: "stop" }
  | { action: "send"; text: string; target: string; rounds: 1 | 2 };
type Attempt = {
  body: RadioRequest;
  acknowledged?: boolean;
  rejected?: boolean;
};
export default function RadioChat({
  room,
  data,
  getDraft,
  subscribeDraft,
  setDraft,
  refresh,
  notify,
}: {
  room: Room;
  data: Snapshot;
  getDraft: DraftReader;
  subscribeDraft: DraftSubscription;
  setDraft: (text: string) => void;
  refresh: () => Promise<void>;
  notify: (message: string) => void;
}) {
  const draft = usePromptDraft(room.id, getDraft, subscribeDraft);
  const radio = room.radio!;
  const key = `studio-radio-command:${data.stateDir}:${room.id}`;
  const [attempt, setAttempt] = useState<Attempt | null>(() =>
    saved(key, null),
  );
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const [target, setTarget] = useState("both");
  const locked = useRef(false);
  const members = room.members
    .map((id) => data.threads.find((a) => a.id === id))
    .filter((a) => !!a);
  const messages = useMessages(room.id, "room", false, data.stateDir);
  const { scroll, content, onScroll } = useConversationScroll(
    room.id,
    messages.loaded,
  );
  const remember = (value: Attempt | null) => {
    save(key, value);
    setAttempt(value);
  };
  const execute = async (next: Attempt) => {
    if (locked.current) return;
    locked.current = true;
    setPending(true);
    setError("");
    remember(next);
    try {
      if (!next.acknowledged && !next.rejected) {
        await post("/api/peer-teams", next.body, { timeoutMs: 15000 });
        next = { ...next, acknowledged: true };
        remember(next);
      }
      if (
        next.acknowledged &&
        next.body.radio_action === "send" &&
        getDraft(room.id) === next.body.text
      )
        setDraft("");
      await refresh();
      messages.reload();
      remember(null);
    } catch (e) {
      if (
        !next.acknowledged &&
        e instanceof ApiError &&
        e.status >= 400 &&
        e.status < 500
      ) {
        next = { ...next, rejected: true };
        remember(next);
      }
      setError(errorText(e));
    } finally {
      locked.current = false;
      setPending(false);
    }
  };
  const command = (input: RadioActionInput) => {
    if (attempt || locked.current) return;
    const path =
      room.projectPath ||
      data.runtime.peerTeams?.find((t) => t.id === radio.teamId)?.projectPath;
    if (!path) {
      notify("The shared chat project is unavailable. Refresh Studio.");
      return;
    }
    const base = {
      action: "radio" as const,
      path,
      team_id: radio.teamId || room.peerTeamId,
      request_id: crypto.randomUUID(),
      expected_revision: radio.revision,
    };
    switch (input.action) {
      case "pass":
        void execute({
          body: {
            ...base,
            radio_action: "pass",
            target: input.target,
          },
        });
        break;
      case "stop":
        void execute({ body: { ...base, radio_action: "stop" } });
        break;
      case "send":
        void execute({
          body: {
            ...base,
            radio_action: "send",
            text: input.text,
            target: input.target,
            rounds: input.rounds,
          },
        });
        break;
    }
  };
  const busy =
    !!radio.active || (radio.status !== "idle" && radio.status !== "blocked");
  const frozen = pending || !!attempt;
  const inputBlocked =
    radio.status === "stopping" ||
    (radio.status === "blocked" && !!radio.active);
  const canSend = !frozen && !inputBlocked && !!draft.trim();
  const requests = data.runtime.requests.filter(
    (r) =>
      r.agent === radio.active?.agentId &&
      (!r.status || r.status === "pending") &&
      (!(r.turnId || r.params?.turnId) ||
        (r.turnId || r.params?.turnId) === radio.active?.turnId),
  );
  return (
    <section className="radio-chat" aria-label="Shared chat">
      <div className="radio-participants">
        {members.map((agent) => (
          <div
            className="radio-participant"
            key={agent.id}
            data-speaking={
              radio.speaker === agent.id && radio.status === "speaking"
            }
          >
            <AgentAvatar id={agent.id} size={28} />
            <div>
              <strong>{agent.name}</strong>
              <small>
                {agent.provider === "claude" ? "Claude" : "Codex"} ·{" "}
                {agent.model}
              </small>
            </div>
            <span className="radio-speaker">
              {radio.speaker === agent.id && radio.status === "speaking"
                ? "Speaking"
                : busy
                  ? "Waiting"
                  : "Ready"}
            </span>
            {busy &&
              radio.status !== "blocked" &&
              radio.speaker !== agent.id && (
                <Button
                  size="compact-xs"
                  variant="subtle"
                  disabled={frozen || radio.status === "stopping"}
                  onClick={() => command({ action: "pass", target: agent.id })}
                >
                  Reply next
                </Button>
              )}
          </div>
        ))}
        {busy && (
          <Button
            size="compact-xs"
            variant="subtle"
            disabled={frozen || radio.status === "stopping"}
            onClick={() => command({ action: "stop" })}
          >
            {radio.status === "stopping" ? "Stopping…" : "Stop"}
          </Button>
        )}
      </div>
      <div className="radio-history" ref={scroll} onScroll={onScroll}>
        <div className="radio-history-content" ref={content}>
          {messages.before != null && (
            <Button
              variant="subtle"
              size="compact-xs"
              disabled={messages.pageLoading}
              onClick={() => void messages.older()}
            >
              Load earlier messages
            </Button>
          )}
          {messages.notice && <p role="status">{messages.notice}</p>}
          {!messages.loaded && <p role="status">Loading messages…</p>}
          {messages.loaded && !messages.items.length && (
            <p className="radio-empty">
              Both agents see this conversation. They reply one at a time.
            </p>
          )}
          {messages.items.map((message) => (
            <article
              className="radio-message"
              data-sender={message.sender}
              data-message={message.id}
              key={message.id}
            >
              <div className="radio-author">
                {message.sender !== "user" && (
                  <AgentAvatar id={message.sender || "agent"} size={24} />
                )}
                <strong>
                  {message.sender === "user"
                    ? "You"
                    : message.senderName ||
                      members.find((a) => a.id === message.sender)?.name ||
                      "Agent"}
                </strong>
                <MessageDate at={message.created} />
              </div>
              <StreamingText
                text={message.text || ""}
                agentId={message.sender}
              />
            </article>
          ))}
          {!!requests.length && (
            <Requests
              requests={requests}
              allRequests={data.runtime.requests}
              agents={data.threads}
              scope={data.stateDir}
              refresh={refresh}
              notify={notify}
            />
          )}
        </div>
      </div>
      <form
        className="radio-composer"
        onSubmit={(e) => {
          e.preventDefault();
          if (canSend)
            command({ action: "send", text: draft, target, rounds: 1 });
        }}
      >
        {radio.error && <p role="alert">{radio.error}</p>}
        {attempt && (
          <div className="radio-retry" role="alert">
            <span>
              {error || "A previous request needs confirmation."}{" "}
              {attempt.acknowledged
                ? "Saved. Refresh to continue."
                : attempt.rejected
                  ? "Request rejected. Refresh to continue."
                  : "The result is unknown. Retry the same request."}
            </span>
            <Button
              size="compact-xs"
              disabled={pending}
              onClick={() => void execute(attempt)}
            >
              {attempt.acknowledged || attempt.rejected ? "Refresh" : "Retry"}
            </Button>
          </div>
        )}
        <Textarea
          aria-label="Message both agents"
          placeholder="Message the shared chat…"
          autosize
          minRows={2}
          maxRows={7}
          value={draft}
          disabled={frozen}
          onChange={(e) => setDraft(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (
              e.key === "Enter" &&
              !e.shiftKey &&
              !e.nativeEvent.isComposing &&
              canSend
            ) {
              e.preventDefault();
              command({ action: "send", text: draft, target, rounds: 1 });
            }
          }}
        />
        <div className="radio-compose-actions">
          <Select
            aria-label="Reply from"
            data={[
              { value: "both", label: "Both agents" },
              ...members.map((a) => ({ value: a.id, label: a.name })),
            ]}
            value={target}
            onChange={(value) => setTarget(value || "both")}
            allowDeselect={false}
            disabled={frozen || inputBlocked}
          />
          <span>
            {busy
              ? radio.status === "blocked"
                ? "Resolve the current reply before sending."
                : radio.status === "stopping"
                  ? "Waiting for the reply to stop."
                  : "Current reply finishes first."
              : target === "both"
                ? "One reply each"
                : "One reply"}
          </span>
          {target === "both" && (
            <Button
              variant="subtle"
              disabled={!canSend}
              title="Two replies each, four replies total"
              onClick={() =>
                command({ action: "send", text: draft, target, rounds: 2 })
              }
            >
              Discuss (4 replies)
            </Button>
          )}
          <Button type="submit" disabled={!canSend}>
            Send
          </Button>
        </div>
      </form>
    </section>
  );
}
