import {
  Badge,
  Button,
  Modal,
  NativeSelect,
  Textarea,
  Tabs,
  UnstyledButton,
} from "@mantine/core";
import { BookOpen, Plus } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import {
  complaintLabel,
  type Snapshot,
  type Json,
  type Complaint,
} from "../types";
import "./complaint-book.css";

const recipient = (c: Json) =>
  c.recipient || (c.author === c.leadId ? "user" : "lead");

function ComplaintResponse({
  detail,
  token,
  requests,
  refresh,
  notify,
  onResponse,
}: {
  detail: Json;
  token: string;
  requests: Map<string, Json>;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
  onResponse: (c: Json) => void;
}) {
  const previous = requests.get(detail.id);
  const [text, setText] = useState(previous?.text || "");
  const [status, setStatus] = useState(previous?.status || "in_progress");
  const [sending, setSending] = useState(false);
  const [retry, setRetry] = useState(!!previous);
  const [error, setError] = useState("");
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (sending || !text.trim()) return;
    const payload = requests.get(detail.id) || {
      id: crypto.randomUUID(),
      action: "respond",
      complaint_id: detail.id,
      version: detail.version,
      text: text.trim(),
      status,
    };
    requests.set(detail.id, payload);
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
          setRetry(false);
          if (
            response.status === 409 ||
            /version|changed|stale/i.test(result.error || "")
          ) {
            setError(
              "This complaint changed. Review the latest response before you send again.",
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
      setRetry(false);
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
      <h3>Your response</h3>
      <NativeSelect
        label="Action"
        value={status}
        disabled={sending || retry}
        onChange={(event) => setStatus(event.target.value)}
      >
        <option value="in_progress">I'll handle it</option>
        <option value="resolved">Resolved</option>
        <option value="declined">Declined</option>
      </NativeSelect>
      <Textarea
        label="Action or reason"
        value={text}
        required
        maxLength={12000}
        autosize
        minRows={3}
        maxRows={8}
        disabled={sending || retry}
        onChange={(event) => setText(event.target.value)}
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
        disabled={!text.trim()}
      >
        {retry ? "Retry response" : "Send response"}
      </Button>
      <p className="notice">
        The orchestrator receives your response as a message.
      </p>
    </form>
  );
}

export default function ComplaintBook({
  data,
  leadId,
  refresh,
  notify,
}: {
  data: Snapshot;
  leadId?: string;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const [tab, setTab] = useState<string | null>("user");
  const responseRequests = useRef(new Map<string, Json>());
  const [filter, setFilter] = useState("pending"),
    [detail, setDetail] = useState<Json | null>(null),
    [detailId, setDetailId] = useState<string | null>(null),
    [create, setCreate] = useState(false),
    [text, setText] = useState(""),
    [selected, setSelected] = useState(leadId || ""),
    [sending, setSending] = useState(false);
  const activeDetailId = useRef(detailId);
  activeDetailId.current = detailId;
  const changeDetail = (id: string | null) => {
    activeDetailId.current = id;
    setDetailId(id);
  };
  const request = useRef<Json | null>(null),
    leads = data.threads.filter((a) => a.source === "managed" && a.isLead),
    records = (data.runtime.complaints || []).map((complaint) =>
      !complaint.recipient && complaint.author === complaint.leadId
        ? { ...complaint, needsResponse: true, status: "open", readAt: null }
        : complaint,
    );
  useEffect(() => {
    if (!detailId) return;
    let live = true;
    api(`/api/complaint?id=${encodeURIComponent(detailId)}`)
      .then((c) => {
        if (live)
          setDetail((current) =>
            current && current.id === c.id && current.version > c.version
              ? current
              : c,
          );
      })
      .catch((e) => notify(errorText(e)));
    return () => {
      live = false;
    };
  }, [detailId, data, notify]);
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (sending) return;
    const lead = selected || leads[0]?.id;
    if (!lead || !text.trim()) return;
    if (request.current?.text !== text.trim() || request.current?.lead !== lead)
      request.current = { id: crypto.randomUUID(), text: text.trim(), lead };
    setSending(true);
    try {
      await api("/api/complaints", request.current);
      request.current = null;
      setText("");
      setCreate(false);
      setTab("lead");
      setFilter("pending");
      await refresh();
    } catch (e) {
      notify(errorText(e));
    } finally {
      setSending(false);
    }
  };
  const shown = records.filter(
    (c) => recipient(c) === tab && (filter === "all" || c.needsResponse),
  );
  const pending = (target: string) =>
    records.filter((c) => recipient(c) === target && c.needsResponse).length;
  const summary = records.find((c) => c.id === detailId);
  const recipientName = (c: Json) =>
    recipient(c) === "user"
      ? "You"
      : c.leadName || summary?.leadName || "Orchestrator";
  const authorName = (c: Json) =>
    c.author === "user"
      ? "You"
      : data.threads.find((a) => a.id === c.author)?.name ||
        c.authorName ||
        c.author;
  return (
    <section id="complaint-book">
      <div className="book-toolbar">
        <p>
          Orchestrator complaints need your response. Worker complaints go to
          their orchestrator.
        </p>
        <Button
          id="new-complaint"
          variant="light"
          color="indigo"
          leftSection={<Plus size={16} />}
          onClick={() => setCreate(true)}
          disabled={!leads.length}
        >
          New complaint
        </Button>
      </div>
      <Tabs value={tab} onChange={setTab} className="complaint-owner-tabs">
        <Tabs.List>
          <Tabs.Tab
            value="user"
            rightSection={
              <Badge
                size="xs"
                variant="light"
                color={pending("user") ? "orange" : "gray"}
              >
                {pending("user")}
              </Badge>
            }
          >
            For you
          </Tabs.Tab>
          <Tabs.Tab
            value="lead"
            rightSection={
              <Badge
                size="xs"
                variant="light"
                color={pending("lead") ? "orange" : "gray"}
              >
                {pending("lead")}
              </Badge>
            }
          >
            For orchestrator
          </Tabs.Tab>
        </Tabs.List>
      </Tabs>
      <div className="book-filter">
        <NativeSelect
          aria-label="Show complaints"
          id="complaint-filter"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        >
          <option value="pending">Needs a response</option>
          <option value="all">All complaints</option>
        </NativeSelect>
      </div>
      <div id="complaint-list">
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
                ? c.readAt
                  ? "Responded by you"
                  : "Awaiting your response"
                : c.readAt
                  ? "Read by orchestrator"
                  : "Not read by orchestrator"}
              {recipient(c) === "lead" && c.leadDeleted
                ? " · Lead deleted"
                : recipient(c) === "lead" && c.leadStopped
                  ? " · Lead stopped"
                  : ""}
            </small>
          </UnstyledButton>
        ))}
      </div>
      {!shown.length && (
        <div className="book-empty">
          <BookOpen size={26} />
          <h2>
            No complaints {filter === "pending" ? "need a response" : "yet"}
          </h2>
          <p>
            {tab === "user"
              ? "Your orchestrators can ask you to resolve a problem here."
              : "Worker and user complaints appear here for the orchestrator."}
          </p>
        </div>
      )}
      <Modal
        opened={!!detailId}
        onClose={() => changeDetail(null)}
        title="Complaint"
      >
        {detail ? (
          <>
            <p className="notice">
              {summary?.authorName || authorName(detail)} →{" "}
              {recipientName(detail)} · {complaintLabel(detail.status)}
            </p>
            <p className="complaint-text">{detail.text}</p>
            <p className="notice">
              {recipient(detail) === "user"
                ? detail.recipient === "user" && detail.readAt
                  ? "You responded to this complaint."
                  : "Your response is required."
                : detail.readAt
                  ? "Read by orchestrator: " +
                    new Date(detail.readAt * 1000).toLocaleString()
                  : "The orchestrator has not read this complaint."}
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
              <p>A response from the orchestrator is required.</p>
            )}
            {detail.recipient === "user" &&
              Number.isInteger(detail.version) && (
                <ComplaintResponse
                  key={detail.id}
                  detail={detail}
                  token={data.token}
                  requests={responseRequests.current}
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
        ) : (
          <p>Loading…</p>
        )}
      </Modal>
      <Modal
        opened={create}
        onClose={() => setCreate(false)}
        title="New complaint"
      >
        <form id="complaint-form" onSubmit={submit}>
          <NativeSelect
            label="Responsible orchestrator"
            id="complaint-lead"
            value={selected || leads[0]?.id}
            onChange={(e) => setSelected(e.target.value)}
          >
            {leads.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
          </NativeSelect>
          <Textarea
            label="What went wrong?"
            id="complaint-text"
            value={text}
            onChange={(e) => setText(e.target.value)}
            maxLength={12000}
            rows={6}
            required
          />
          <p className="notice">
            The orchestrator receives a message and must respond. A stopped
            orchestrator waits until you resume it.
          </p>
          <Button
            id="submit-complaint"
            variant="filled"
            color="indigo"
            type="submit"
            disabled={sending}
          >
            Submit complaint
          </Button>
        </form>
      </Modal>
    </section>
  );
}
