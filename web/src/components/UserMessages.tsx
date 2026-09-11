import {
  Badge,
  Button,
  Modal,
  NativeSelect,
  Textarea,
  UnstyledButton,
} from "@mantine/core";
import { MessageSquare } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText, save, saved } from "../api";
import {
  complaintLabel,
  type Snapshot,
  type Json,
  type Complaint,
} from "../types";
import "./complaint-book.css";

// Keep unsent replies in memory, separate from durable retry receipts.
const replyDrafts = new Map<string, { text: string; status: string }>();

const recipient = (c: Json) =>
  c.recipient || (c.author === c.leadId ? "user" : "lead");

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
  const draft = replyDrafts.get(draftKey);
  const previous = requests.get(detail.id);
  const [text, setText] = useState(previous?.text || draft?.text || "");
  const [status, setStatus] = useState(
    previous?.status || draft?.status || "in_progress",
  );
  const [resolution, setResolution] = useState(false);
  const [sending, setSending] = useState(false);
  const [retry, setRetry] = useState(!!previous);
  const [error, setError] = useState("");
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
    save(pendingKey, Object.fromEntries(requests));
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
      <Button
        type="button"
        variant="subtle"
        size="compact-xs"
        onClick={() => setResolution(!resolution)}
        aria-expanded={resolution}
      >
        Change message status (optional)
      </Button>
      {(resolution || status !== "in_progress") && (
        <NativeSelect
          label="Message status"
          value={status}
          disabled={sending || retry}
          onChange={(event) => {
            setStatus(event.target.value);
            replyDrafts.set(draftKey, { text, status: event.target.value });
          }}
        >
          <option value="in_progress">Keep open</option>
          <option value="resolved">Resolved</option>
          <option value="declined">Declined</option>
        </NativeSelect>
      )}
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
          replyDrafts.set(draftKey, { text: event.target.value, status });
        }}
      />
      {error && (
        <p className="complaint-reply-error" role="alert">
          {error}
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
  const responseRequests = useRef(
    new Map<string, Json>(Object.entries(saved(pendingKey, {}))),
  );
  const [filter, setFilter] = useState(target === "lead" ? "all" : "pending"),
    [detail, setDetail] = useState<Json | null>(null),
    [detailId, setDetailId] = useState<string | null>(null),
    [detailError, setDetailError] = useState(""),
    [detailAttempt, setDetailAttempt] = useState(0);
  const activeDetailId = useRef(detailId);
  activeDetailId.current = detailId;
  const changeDetail = (id: string | null) => {
    activeDetailId.current = id;
    setDetailError("");
    setDetailId(id);
  };
  const records = data.runtime.complaints || [];
  useEffect(() => {
    if (
      !focusId ||
      !records.some((item) => item.id === focusId && recipient(item) === target)
    )
      return;
    setDetail(null);
    changeDetail(focusId);
  }, [focusId, focusRequestId]);
  useEffect(() => {
    if (!detailId) return;
    let live = true;
    api(`/api/complaint?id=${encodeURIComponent(detailId)}`)
      .then((c) => {
        if (live) setDetailError("");
        if (live)
          setDetail((current) =>
            current && current.id === c.id && current.version > c.version
              ? current
              : c,
          );
      })
      .catch((e) => {
        if (live) setDetailError(errorText(e));
      });
    return () => {
      live = false;
    };
  }, [detailId, detailAttempt, data]);
  const shown = records.filter(
    (c) => recipient(c) === target && (filter === "all" || c.needsResponse),
  );
  const summary = records.find((c) => c.id === detailId);
  const recipientName = (c: Json) =>
    recipient(c) === "user"
      ? "You"
      : c.leadName || summary?.leadName || "Main agent";
  const authorName = (c: Json) =>
    c.author === "user"
      ? "You"
      : data.threads.find((a) => a.id === c.author)?.name ||
        c.authorName ||
        c.author;
  if (
    target === "lead" &&
    !records.some((record) => recipient(record) === "lead")
  )
    return null;
  return (
    <section className="user-message-list">
      {records.some((c) => recipient(c) === target) && (
        <div className="book-filter">
          <NativeSelect
            aria-label={
              target === "user"
                ? "Show messages to you"
                : "Show messages to main agent"
            }
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          >
            <option value="pending">Needs a response</option>
            <option value="all">All messages</option>
          </NativeSelect>
        </div>
      )}
      <div className="user-message-rows">
        {shown.map((c: Complaint) => (
          <UnstyledButton
            className="complaint-card"
            key={c.id}
            data-complaint={c.id}
            onClick={() => {
              setDetail(null);
              changeDetail(c.id);
            }}
          >
            <span className="complaint-meta">
              <Badge
                variant="light"
                color={c.needsResponse ? "orange" : "gray"}
              >
                {complaintLabel(c.status)}
              </Badge>
              <span>
                {c.authorName} → {recipientName(c)}
              </span>
            </span>
            <span className="complaint-title">{c.title}</span>
            <small>
              {new Date(c.created * 1000).toLocaleString()} ·{" "}
              {recipient(c) === "user"
                ? !c.needsResponse
                  ? "Responded by you"
                  : "Awaiting your response"
                : c.readAt
                  ? "Read by main agent"
                  : "Not read by main agent"}
              {recipient(c) === "lead" && c.leadDeleted
                ? " · Main agent deleted"
                : recipient(c) === "lead" && c.leadStopped
                  ? " · Main agent stopped"
                  : ""}
            </small>
          </UnstyledButton>
        ))}
      </div>
      {!hideEmpty && !shown.length && (
        <div className="book-empty">
          <MessageSquare size={26} />
          <h2>
            No messages {filter === "pending" ? "need a response" : "yet"}
          </h2>
          <p>
            {target === "user"
              ? "The main agent can send you a message here."
              : "Subagent requests appear here for the main agent."}
          </p>
        </div>
      )}
      <Modal
        opened={!!detailId}
        onClose={() => changeDetail(null)}
        title={target === "user" ? "Message to you" : "Message to main agent"}
      >
        {detailError && (
          <div role="alert">
            <p>Could not load the message: {detailError}</p>
            <Button
              onClick={() => {
                setDetailError("");
                setDetailAttempt((value) => value + 1);
              }}
            >
              Retry
            </Button>
          </div>
        )}
        {detail ? (
          <>
            <p className="notice">
              {summary?.authorName || authorName(detail)} →{" "}
              {recipientName(detail)} · {complaintLabel(detail.status)}
            </p>
            <p className="complaint-text">{detail.text}</p>
            <p className="notice">
              {recipient(detail) === "user"
                ? detail.responses.some(
                    (response: Json) => response.author === "user",
                  )
                  ? "You responded to this message."
                  : "Your response is required."
                : detail.readAt
                  ? "Read by main agent: " +
                    new Date(detail.readAt * 1000).toLocaleString()
                  : "The main agent has not read this message."}
            </p>
            {detail.responses.map((r: Json) => (
              <article className="complaint-response" key={r.id}>
                <strong>
                  {authorName({ author: r.author || detail.leadId })} ·{" "}
                  {complaintLabel(r.status)}
                </strong>
                <small>{new Date(r.at * 1000).toLocaleString()}</small>
                <p className="complaint-text">{r.text}</p>
              </article>
            ))}
            {recipient(detail) === "lead" && !detail.responses.length && (
              <p>A response from the main agent is required.</p>
            )}
            {detail.recipient === "user" &&
              Number.isInteger(detail.version) && (
                <MessageResponse
                  key={detail.id}
                  detail={detail}
                  token={data.token}
                  requests={responseRequests.current}
                  pendingKey={pendingKey}
                  refresh={refresh}
                  notify={notify}
                  onResponse={(result) => {
                    if (
                      activeDetailId.current !== detail.id ||
                      result.id !== detail.id
                    )
                      return;
                    setDetail((current) => {
                      if (activeDetailId.current !== result.id) return current;
                      return current &&
                        current.id === result.id &&
                        current.version > result.version
                        ? current
                        : result;
                    });
                  }}
                />
              )}
            {recipient(detail) === "user" &&
              (detail.recipient !== "user" ||
                !Number.isInteger(detail.version)) && (
                <p className="notice">Update the server to respond here.</p>
              )}
          </>
        ) : !detailError ? (
          <p role="status">Loading…</p>
        ) : null}
      </Modal>
    </section>
  );
}
