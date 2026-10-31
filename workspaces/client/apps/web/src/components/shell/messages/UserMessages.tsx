import { useSubscription } from "../../../sync/subscriptions";
import {
  useServerApi,
  useServerStorage,
} from "../../../servers/ServerServicesContext";
import { Button, Textarea } from "@mantine/core";
import { useEffect, useMemo, useState } from "react";
import { ApiError, errorText } from "../../../api";
import type { Complaint, Snapshot } from "../../../types";
import type { paths } from "../../../generated/api";
import "./complaint-book.css";
import ErrorDescription from "../../ErrorDescription";
import MessageDate from "../../conversation/transcript/MessageDate";
import StreamingText from "../../conversation/transcript/StreamingText";
import AgentAvatar from "../../agents/AgentAvatar";

import {
  complaintReplyRequest,
  complaintResponseStore,
  isDefinitiveComplaintRefusal,
  submitComplaintResponse,
  type UserComplaintResponse,
} from "./complaintReplyRequest";

// Preserve confirmed replies while a cached snapshot catches up.
type ComplaintDetailResponse =
  paths["/api/complaint"]["get"]["responses"][200]["content"]["application/json"];
type ComplaintSummary = Complaint;
type ReplyDraft = Pick<UserComplaintResponse, "text" | "status">;

const caches = new WeakMap<
  Storage,
  {
    confirmedMessages: Map<string, ComplaintDetailResponse>;
    replyDrafts: Map<string, ReplyDraft>;
  }
>();
function useMessageCache() {
  const storage = useServerStorage();
  let cache = caches.get(storage);
  if (!cache) {
    cache = { confirmedMessages: new Map(), replyDrafts: new Map() };
    caches.set(storage, cache);
  }
  return cache;
}

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
  const { get, post } = useServerApi();
  const storage = useServerStorage();
  const responseStore = complaintResponseStore(
    storage,
    pendingKey.slice("studio-message-responses:".length),
  );
  const { replyDrafts } = useMessageCache();

  const draftKey = JSON.stringify([pendingKey, detail.id]);
  const draft = replyDrafts.get(draftKey);
  const previous = requests.get(detail.id);
  const [text, setText] = useState(previous?.text || draft?.text || "");
  const status: UserComplaintResponse["status"] =
    previous?.status || "in_progress";
  const [sending, setSending] = useState(false);
  const [retry, setRetry] = useState(!!previous);
  const [error, setError] = useState<unknown>(null);
  const saveDraft = (value: ReplyDraft) => {
    replyDrafts.set(draftKey, value);
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
    const existing = requests.get(detail.id);
    requests.set(detail.id, payload);
    try {
      responseStore.write(requests);
    } catch (failure) {
      if (existing) requests.set(detail.id, existing);
      else requests.delete(detail.id);
      setError(errorText(failure));
      return;
    }
    setSending(true);
    setError("");
    try {
      const outcome = await submitComplaintResponse(
        payload,
        token,
        (request, sessionToken) =>
          post("/api/complaints", request, { sessionToken }),
        (id) => get("/api/complaint", { query: { id } }),
      );
      const remaining = new Map(requests);
      remaining.delete(detail.id);
      responseStore.write(remaining);
      requests.delete(detail.id);
      setRetry(false);
      replyDrafts.delete(draftKey);
      setText("");
      if ("detail" in outcome) onResponse(outcome.detail);
      else notify(errorText(outcome.reloadError));
      try {
        await refresh();
      } catch (error) {
        notify(errorText(error));
      }
    } catch (failure) {
      if (failure instanceof ApiError) {
        if (isDefinitiveComplaintRefusal(failure.status)) {
          const remaining = new Map(requests);
          remaining.delete(detail.id);
          try {
            responseStore.write(remaining);
          } catch (storageFailure) {
            setError(errorText(storageFailure));
            return;
          }
          requests.delete(detail.id);
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
  focusId?: string | undefined;
  onlyComplaintId?: string;
  focusRequestId?: string | undefined;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const storage = useServerStorage();
  const pendingKey = `studio-message-responses:${data.stateDir}`;
  const pending = useMemo(() => {
    try {
      return {
        requests: new Map(
          Object.entries(complaintResponseStore(storage, data.stateDir).read()),
        ),
        error: "",
      };
    } catch {
      return {
        requests: new Map<string, UserComplaintResponse>(),
        error:
          "The saved response request cannot be read. Resolve it before sending another response.",
      };
    }
  }, [pendingKey, storage]);
  const records = data.runtime.complaints.filter(
    (message) =>
      recipient(message) === target &&
      (!onlyComplaintId || message.id === onlyComplaintId),
  );
  return (
    <section className="user-message-list">
      {pending.error ? (
        <p role="alert">{pending.error}</p>
      ) : (
        records.map((message) => (
          <UserMessage
            key={JSON.stringify([data.stateDir, message.id])}
            message={message}
            data={data}
            focused={message.id === focusId}
            focusRequestId={focusRequestId}
            requests={pending.requests}
            pendingKey={pendingKey}
            refresh={refresh}
            notify={notify}
          />
        ))
      )}
      {!hideEmpty && !records.length && (
        <p className="notice">No messages yet.</p>
      )}
    </section>
  );
}

export function UserMessage({
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
  focusRequestId?: string | undefined;
  requests: Map<string, UserComplaintResponse>;
  pendingKey: string;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const { get } = useServerApi();
  const { confirmedMessages } = useMessageCache();

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
  const streamed = useSubscription({
    kind: "complaint",
    complaintId: message.id,
  });
  useEffect(() => {
    if (focused) setReply(true);
  }, [focused, focusRequestId]);
  useEffect(() => {
    if (isComplaintDetail(message)) {
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
  }, [message, confirmedKey, confirmedMessages]);
  useEffect(() => {
    if (streamed.data) {
      setDetail((current) =>
        (current.version || 0) > (streamed.data!.version || 0)
          ? current
          : streamed.data!,
      );
      setError(null);
    }
    if (streamed.error) setError(streamed.error);
  }, [streamed.data, streamed.error]);
  const retryDetail = () => {
    void get("/api/complaint", { query: { id: message.id } })
      .then(setDetail)
      .catch(setError);
  };
  const detailView = isComplaintDetail(detail) ? detail : null;
  const authorName = (id: string) =>
    id === "user"
      ? "You"
      : data.threads.find((agent) => agent.id === id)?.name ||
        message.authorName ||
        id;
  return (
    <article className="message-inline-thread" data-complaint={message.id}>
      <div className="chat-message-author">
        <AgentAvatar id={message.author || ""} size={24} />
        <strong>{authorName(message.author || "")}</strong>
        {message.recipient === "lead" && (
          <span>to {message.leadName || "Main agent"}</span>
        )}
      </div>
      <MessageDate at={detailView?.created ?? message.created} />
      <StreamingText
        text={detailView?.text ?? ""}
        agentId={message.author || undefined}
      />
      {!detailView && !error && <p role="status">Loading message…</p>}
      {!!error && (
        <p role="alert">
          <ErrorDescription value={error} role="status" />{" "}
          <Button onClick={retryDetail}>Retry</Button>
        </p>
      )}
      {detailView?.responses.map((response) => (
        <article className="complaint-response" key={response.id}>
          <strong>
            {authorName(response.author || detailView.leadId || "")}
          </strong>
          <MessageDate at={response.at} />
          <StreamingText
            text={response.text || ""}
            agentId={response.author || undefined}
          />
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
  return "text" in complaint;
}
