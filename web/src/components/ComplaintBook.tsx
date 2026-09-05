import {
  Badge,
  Button,
  Modal,
  NativeSelect,
  Textarea,
  UnstyledButton,
} from "@mantine/core";
import { BookOpen, Plus } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import { complaintLabel, type Snapshot, type Json } from "../types";
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
  const [filter, setFilter] = useState("pending"),
    [detail, setDetail] = useState<Json | null>(null),
    [detailId, setDetailId] = useState<string | null>(null),
    [create, setCreate] = useState(false),
    [text, setText] = useState(""),
    [selected, setSelected] = useState(leadId || ""),
    [sending, setSending] = useState(false);
  const request = useRef<Json | null>(null),
    leads = data.threads.filter((a) => a.source === "managed" && a.isLead),
    records = data.runtime.complaints || [];
  useEffect(() => {
    if (!detailId) return;
    let live = true;
    api(`/api/complaint?id=${encodeURIComponent(detailId)}`)
      .then((c) => {
        if (live) setDetail(c);
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
      await refresh();
    } catch (e) {
      notify(errorText(e));
    } finally {
      setSending(false);
    }
  };
  return (
    <section id="complaint-book">
      <div className="book-toolbar">
        <p>
          Problems, decisions, and actions. Every complaint requires a lead
          response.
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
        {records
          .filter((c) => filter === "all" || c.needsResponse)
          .map((c) => (
            <UnstyledButton
              className="complaint-card"
              key={c.id}
              data-complaint={c.id}
              onClick={() => {
                setDetail(null);
                setDetailId(c.id);
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
                  {c.authorName} → {c.leadName}
                </span>
              </span>
              <span className="complaint-title">{c.title}</span>
              <small>
                {new Date(c.created * 1000).toLocaleString()} ·{" "}
                {c.readAt ? "Read by lead" : "Not read by lead"}
                {c.leadDeleted
                  ? " · Lead deleted"
                  : c.leadStopped
                    ? " · Lead stopped"
                    : ""}
              </small>
            </UnstyledButton>
          ))}
      </div>
      {!records.some((c) => filter === "all" || c.needsResponse) && (
        <div className="book-empty">
          <BookOpen size={26} />
          <h2>
            No complaints {filter === "pending" ? "need a response" : "yet"}
          </h2>
          <p>Problems reported by you or your agents appear here.</p>
        </div>
      )}
      <Modal
        opened={!!detailId}
        onClose={() => setDetailId(null)}
        title="Complaint"
      >
        {detail ? (
          <>
            <p className="notice">
              {records.find((c) => c.id === detail.id)?.authorName} →{" "}
              {records.find((c) => c.id === detail.id)?.leadName} ·{" "}
              {complaintLabel(detail.status)}
            </p>
            <p className="complaint-text">{detail.text}</p>
            <p className="notice">
              {detail.readAt
                ? "Read by lead: " +
                  new Date(detail.readAt * 1000).toLocaleString()
                : "The lead has not read this complaint."}
            </p>
            {detail.responses.map((r: Json) => (
              <article className="complaint-response" key={r.id}>
                <strong>{complaintLabel(r.status)}</strong>
                <small>{new Date(r.at * 1000).toLocaleString()}</small>
                <p className="complaint-text">{r.text}</p>
              </article>
            ))}
            {!detail.responses.length && (
              <p>A response from the lead is required.</p>
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
            label="Responsible lead"
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
            The lead must read and respond. A stopped lead waits until you
            resume it.
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
