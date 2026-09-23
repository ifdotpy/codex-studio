import { Button, Textarea } from "@mantine/core";
import { useEffect, useMemo, useState } from "react";
import { api, errorText, save, saved } from "../api";
import { type Snapshot, type Json, type Complaint } from "../types";
import "./complaint-book.css";
import ErrorDescription from "./ErrorDescription";
import MessageDate from "./MessageDate";
import StreamingText from "./StreamingText";
import AgentAvatar from "./AgentAvatar";
import { writeLocalDraft } from "../sync/localDraft";

// Preserve confirmed replies while a cached snapshot catches up.
const confirmedMessages = new Map<string, Json>();
// Keep unsent replies separate from immutable delivery attempts.
const replyDrafts = new Map<string, { text: string; status: string }>();

const recipient = (c: Json) => c.recipient;

function MessageResponse({
  detail,
  token,
  requests,
  pendingKey,
  refresh,
  notify,
  onResponse,
}: {
  detail: Json;
  token: string;
  requests: Map<string, Json>;
  pendingKey: string;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
  onResponse: (c: Json) => void;
}) {
  const draftKey = JSON.stringify([pendingKey, detail.id]);
  const storageKey = `studio-reply-draft:${draftKey}`;
  const draft =
    replyDrafts.get(draftKey) ||
    saved<{ text: string; status: string } | null>(storageKey, null);
  const previous = requests.get(detail.id);
  const [text, setText] = useState(previous?.text || draft?.text || "");
  const status = previous?.status || "in_progress";
  const [sending, setSending] = useState(false);
  const [retry, setRetry] = useState(!!previous);
  const [error, setError] = useState<unknown>(null);
  const saveDraft = (value: { text: string; status: string }) => {
    replyDrafts.set(draftKey, value);
    setError(writeLocalDraft(storageKey, value) || "");
  };
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (sending || !text.trim() || text.length > 12000) return;
    const payload = requests.get(detail.id) || {
      id: crypto.randomUUID(),
      action: "respond",
      complaint_id: detail.id,
      version: detail.version,
      text: text.trim(),
      status,
    };
    requests.set(detail.id, payload);
    const storageError = writeLocalDraft(
      pendingKey,
      Object.fromEntries(requests),
    );
    if (storageError) {
      requests.delete(detail.id);
      setError(storageError);
      return;
    }
    setSending(true);
    setError("");
    try {
      const response = await fetch("/api/complaints", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": token,
        },
        body: JSON.stringify(payload),
      });
      const result = await response.json();
      if (!response.ok) {
        if (response.status >= 400 && response.status < 500) {
          requests.delete(detail.id);
          save(pendingKey, Object.fromEntries(requests));
          setRetry(false);
          if (
            response.status === 409 ||
            /version|changed|stale/i.test(result.error || "")
          ) {
            setError(
              "This message changed. Review the latest response before you send again.",
            );
            await refresh();
            onResponse(
              await api(`/api/complaint?id=${encodeURIComponent(detail.id)}`),
            );
            return;
          }
          setError(result.error || "The response was rejected.");
          return;
        }
        throw new Error(
          result.error || "The server could not confirm your response.",
        );
      }
      requests.delete(detail.id);
      save(pendingKey, Object.fromEntries(requests));
      setRetry(false);
      replyDrafts.delete(draftKey);
      const storageError = writeLocalDraft(storageKey, null);
      if (storageError) notify(storageError);
      setText("");
      onResponse(result);
      try {
        await refresh();
      } catch (error) {
        notify(errorText(error));
      }
    } catch (error) {
      const uncertain = requests.has(detail.id);
      setRetry(uncertain);
      setError(
        `${errorText(error)}${uncertain ? " Retry sends the same response." : ""}`,
      );
    } finally {
      setSending(false);
    }
  };
  return (
    <form className="complaint-reply" onSubmit={submit}>
      <h3>Your reply</h3>
      <Textarea
        label="Reply"
        aria-label="Reply"
        value={text}
        required
        error={
          text.length > 12000
            ? "The reply exceeds 12,000 characters. Shorten it before you send."
            : undefined
        }
        autosize
        minRows={3}
        maxRows={8}
        disabled={sending || retry}
        onChange={(event) => {
          setText(event.target.value);
          saveDraft({ text: event.target.value, status });
        }}
      />
      {!!error && (
        <p className="complaint-reply-error" role="alert">
          <ErrorDescription value={error} role="status" />
        </p>
      )}
      <Button
        type="submit"
        variant="filled"
        color="indigo"
        loading={sending}
        disabled={!text.trim() || text.length > 12000}
      >
        {retry ? "Retry response" : "Send reply"}
      </Button>
      <p className="notice">The agent receives your response as a message.</p>
    </form>
  );
}

export default function UserMessages({
  data,
  target = "user",
  hideEmpty = false,
  focusId,
  focusRequestId,
  refresh,
  notify,
}: {
  data: Snapshot;
  target?: "user" | "lead";
  hideEmpty?: boolean;
  focusId?: string;
  focusRequestId?: string;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const pendingKey = `studio-message-responses:${data.stateDir}`;
  const responseRequests = useMemo(
    () => new Map<string, Json>(Object.entries(saved(pendingKey, {}))),
    [pendingKey],
  );
  const records = data.runtime.complaints.filter(
    (message) => recipient(message) === target,
  );
  return (
    <section className="user-message-list">
      {records.map((message) => (
        <UserMessage
          key={JSON.stringify([data.stateDir, message.id])}
          message={message}
          data={data}
          focused={message.id === focusId}
          focusRequestId={focusRequestId}
          requests={responseRequests}
          pendingKey={pendingKey}
          refresh={refresh}
          notify={notify}
        />
      ))}
      {!hideEmpty && !records.length && (
        <p className="notice">No messages yet.</p>
      )}
    </section>
  );
}

function UserMessage({
  message,
  data,
  focused,
  focusRequestId,
  requests,
  pendingKey,
  refresh,
  notify,
}: {
  message: Complaint;
  data: Snapshot;
  focused: boolean;
  focusRequestId?: string;
  requests: Map<string, Json>;
  pendingKey: string;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const confirmedKey = JSON.stringify([data.stateDir, message.id]);
  const [detail, setDetail] = useState<Json>(() => {
    const confirmed = confirmedMessages.get(confirmedKey);
    return confirmed && confirmed.version > (message.version || 0)
      ? confirmed
      : message;
  });
  const [reply, setReply] = useState(focused || requests.has(message.id));
  const [error, setError] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    if (focused) setReply(true);
  }, [focused, focusRequestId]);
  useEffect(() => {
    if (typeof message.text === "string" && Array.isArray(message.responses)) {
      setDetail((current) =>
        (current.version || 0) > (message.version || 0) ? current : message,
      );
      if (
        (confirmedMessages.get(confirmedKey)?.version || 0) <=
        (message.version || 0)
      )
        confirmedMessages.delete(confirmedKey);
      return;
    }
    let live = true;
    api(`/api/complaint?id=${encodeURIComponent(message.id)}`)
      .then((result) => {
        if (live) {
          setDetail(result);
          setError("");
        }
      })
      .catch((reason) => {
        if (live) setError(reason);
      });
    return () => {
      live = false;
    };
  }, [message.id, message.version, message.text, message.responses, attempt]);
  const authorName = (id: string) =>
    id === "user"
      ? "You"
      : data.threads.find((agent) => agent.id === id)?.name ||
        message.authorName ||
        id;
  return (
    <article className="message-inline-thread" data-complaint={message.id}>
      <div className="chat-message-author">
        <AgentAvatar id={message.author} size={24} />
        <strong>{authorName(message.author)}</strong>
        {message.recipient === "lead" && (
          <span>to {message.leadName || "Main agent"}</span>
        )}
      </div>
      <MessageDate at={detail.created || message.created} />
      <StreamingText text={detail.text || ""} agentId={message.author} />
      {typeof detail.text !== "string" && !error && (
        <p role="status">Loading message…</p>
      )}
      {!!error && (
        <p role="alert">
          <ErrorDescription value={error} role="status" />{" "}
          <Button onClick={() => setAttempt((value) => value + 1)}>
            Retry
          </Button>
        </p>
      )}
      {(detail.responses || []).map((response: Json) => (
        <article className="complaint-response" key={response.id}>
          <strong>{authorName(response.author || detail.leadId)}</strong>
          <MessageDate at={response.at} />
          <StreamingText text={response.text || ""} agentId={response.author} />
        </article>
      ))}
      {detail.recipient === "user" && Number.isInteger(detail.version) && (
        <>
          <Button
            variant="subtle"
            size="compact-xs"
            aria-expanded={reply}
            onClick={() => setReply(!reply)}
          >
            {reply ? "Close reply" : "Reply"}
          </Button>
          {reply && (
            <MessageResponse
              detail={detail}
              token={data.token}
              requests={requests}
              pendingKey={pendingKey}
              refresh={refresh}
              notify={notify}
              onResponse={(result) => {
                const cached = confirmedMessages.get(confirmedKey);
                if (!cached || cached.version <= result.version)
                  confirmedMessages.set(confirmedKey, result);
                setDetail((current) =>
                  (current.version || 0) > (result.version || 0)
                    ? current
                    : result,
                );
              }}
            />
          )}
        </>
      )}
    </article>
  );
}
