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
        <button
          id="new-complaint"
          onClick={() => setCreate(true)}
          disabled={!leads.length}
        >
          New complaint
        </button>
      </div>
      <label className="book-filter">
        Show{" "}
        <select
          id="complaint-filter"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        >
          <option value="pending">Needs a response</option>
          <option value="all">All complaints</option>
        </select>
      </label>
      <div id="complaint-list">
        {records
          .filter((c) => filter === "all" || c.needsResponse)
          .map((c) => (
            <button
              className="complaint-card"
              key={c.id}
              data-complaint={c.id}
              onClick={() => {
                setDetail(null);
                setDetailId(c.id);
              }}
            >
              <span className="complaint-meta">
                <strong>{complaintLabel(c.status)}</strong>
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
            </button>
          ))}
      </div>
      {detailId && (
        <div className="modal-backdrop" onClick={() => setDetailId(null)}>
          <section
            role="dialog"
            aria-modal="true"
            aria-label="Complaint"
            className="modal"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="dialog-heading">
              <h2>Complaint</h2>
              <button aria-label="Close" onClick={() => setDetailId(null)}>
                ×
              </button>
            </div>
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
          </section>
        </div>
      )}
      {create && (
        <div className="modal-backdrop">
          <form
            id="complaint-form"
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-label="New complaint"
            onSubmit={submit}
          >
            <div className="dialog-heading">
              <h2>New complaint</h2>
              <button
                type="button"
                aria-label="Close"
                onClick={() => setCreate(false)}
              >
                ×
              </button>
            </div>
            <label>
              Responsible lead
              <select
                id="complaint-lead"
                value={selected || leads[0]?.id}
                onChange={(e) => setSelected(e.target.value)}
              >
                {leads.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name}
                  </option>
                ))}
              </select>
            </label>
            <label>
              What went wrong?
              <textarea
                id="complaint-text"
                value={text}
                onChange={(e) => setText(e.target.value)}
                maxLength={12000}
                rows={6}
                required
              />
            </label>
            <p className="notice">
              The lead must read and respond. A stopped lead waits until you
              resume it.
            </p>
            <button
              id="submit-complaint"
              className="primary"
              disabled={sending}
            >
              Submit complaint
            </button>
          </form>
        </div>
      )}
    </section>
  );
}
