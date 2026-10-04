import { Button, Textarea } from "@mantine/core";
import { useEffect, useMemo, useState } from "react";
import { get, post, ApiError, errorText, save, saved } from "../../../api";
import type { Complaint, Snapshot } from "../../../types";
import type { paths } from "../../../generated/api";
import "./complaint-book.css";
import ErrorDescription from "../../ErrorDescription";
import MessageDate from "../../conversation/transcript/MessageDate";
import StreamingText from "../../conversation/transcript/StreamingText";
import AgentAvatar from "../../agents/AgentAvatar";
import { writeLocalDraft } from "../../../sync/localDraft";
import {
  complaintReplyRequest,
  type UserComplaintResponse,
} from "./complaintReplyRequest";

// Preserve confirmed replies while a cached snapshot catches up.
type ComplaintDetailResponse =
  paths["/api/complaint"]["get"]["responses"][200]["content"]["application/json"];
type ComplaintSummary = Snapshot["runtime"]["complaints"][number];

const confirmedMessages = new Map<string, ComplaintDetailResponse>();
// Keep unsent replies separate from immutable delivery attempts.
const replyDrafts = new Map<string, { text: string; status: string }>();

const recipient = (c: ComplaintSummary) => c.recipient;

function MessageResponse({
  detail,
  token,
  requests,
  pendingKey,
  refresh,
  notify,
  onResponse,
}: {
  detail: ComplaintDetailResponse;
  token: string;
  requests: Map<string, UserComplaintResponse>;
  pendingKey: string;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
  onResponse: (c: ComplaintDetailResponse) => void;
}) {
  const draftKey = JSON.stringify([pendingKey, detail.id]);
  const storageKey = `studio-reply-draft:${draftKey}`;
  const draft =
    replyDrafts.get(draftKey) ||
    saved<{ text: string; status: string } | null>(storageKey, null);
  const previous = requests.get(detail.id);
  const [text, setText] = useState(previous?.text || draft?.text || "");
  const status: UserComplaintResponse["status"] =
    previous?.status || "in_progress";
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
    const payload = complaintReplyRequest(
      requests.get(detail.id),
      detail,
      text.trim(),
      status,
    );
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
      const result = await post("/api/complaints", payload, {
        sessionToken: token,
      });
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
    } catch (failure) {
      if (failure instanceof ApiError) {
        if (failure.status >= 400 && failure.status < 500) {
          requests.delete(detail.id);
          save(pendingKey, Object.fromEntries(requests));
          setRetry(false);
          if (
            failure.status === 409 ||
            /version|changed|stale/i.test(failure.message)
          ) {
            setError(
              "This message changed. Review the latest response before you send again.",
            );
            try {
              await refresh();
              onResponse(
                await get("/api/complaint", { query: { id: detail.id } }),
              );
            } catch (refreshFailure) {
              setError(errorText(refreshFailure));
            }
            return;
          }
          setError(errorText(failure) || "The response was rejected.");
          return;
        }
      }
      const uncertain = requests.has(detail.id);
      setRetry(uncertain);
      setError(
        `${errorText(failure)}${uncertain ? " Retry sends the same response." : ""}`,
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
  onlyComplaintId,
  focusRequestId,
  refresh,
  notify,
}: {
  data: Snapshot;
  target?: "user" | "lead";
  hideEmpty?: boolean;
  focusId?: string;
  onlyComplaintId?: string;
  focusRequestId?: string;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const pendingKey = `studio-message-responses:${data.stateDir}`;
  const responseRequests = useMemo(
    () =>
      new Map<string, UserComplaintResponse>(
        Object.entries(
          saved<Record<string, UserComplaintResponse>>(pendingKey, {}),
        ),
      ),
    [pendingKey],
  );
  const records = (data.runtime?.complaints ?? []).filter(
    (message) =>
      recipient(message) === target &&
      (!onlyComplaintId || message.id === onlyComplaintId),
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
  requests: Map<string, UserComplaintResponse>;
  pendingKey: string;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const confirmedKey = JSON.stringify([data.stateDir, message.id]);
  const [detail, setDetail] = useState<
    ComplaintSummary | ComplaintDetailResponse
  >(() => {
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
    get("/api/complaint", { query: { id: message.id } })
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
      {(detail.responses || []).map((response) => (
        <article className="complaint-response" key={response.id}>
          <strong>{authorName(response.author || detail.leadId)}</strong>
          <MessageDate at={response.at} />
          <StreamingText text={response.text || ""} agentId={response.author} />
        </article>
      ))}
      {isComplaintDetail(detail) && detail.recipient === "user" && (
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

function isComplaintDetail(
  complaint: ComplaintSummary | ComplaintDetailResponse,
): complaint is ComplaintDetailResponse {
  return (
    typeof complaint.text === "string" &&
    Array.isArray(complaint.responses) &&
    Number.isInteger(complaint.version)
  );
}
