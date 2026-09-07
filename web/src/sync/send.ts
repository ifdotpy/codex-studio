import { useEffect, useState } from "react";
import { api, ApiError, errorText, setToken } from "../api";
import { syncDatabase, UnsupportedSyncError } from "./client";

type Intention = {
  body: Record<string, any>;
  status: "queued" | "accepted" | "uncertain" | "failed";
  error?: string;
  receipt?: any;
  created: number;
  displayPending?: boolean;
  attachments?: any[];
  displayText?: string;
};
export type OutboxEntry = Intention & { id: string };
export type OutgoingMessage = Omit<OutboxEntry, "status"> & {
  status: OutboxEntry["status"] | "sending";
};
const active = new Map<string, Promise<any>>();
const updateIntention = (doc: any, change: Partial<Intention>) =>
  doc.incrementalModify((record: any) => ({
    ...record,
    payload: JSON.stringify({ ...JSON.parse(record.payload), ...change }),
  }));
async function deliver(doc: any) {
  const value: Intention = JSON.parse(doc.payload);
  if (value.status !== "queued")
    return value.receipt || { status: value.status, error: value.error };
  try {
    const { workspaceId } = await syncDatabase();
    const identity = await api<{ workspaceId: string }>("/api/sync/identity");
    if (identity.workspaceId !== workspaceId)
      throw new ApiError(
        "The server workspace changed. Reload before sending.",
        409,
      );
    const state = await api("/api/state");
    setToken(state.token);
    const result = await api<any>("/api/messages", value.body);
    if (result.status === "failed" || result.status === "cancelled")
      throw new ApiError(
        result.error ||
          (result.status === "cancelled"
            ? "This message was cancelled."
            : "The message was not sent."),
        400,
      );
    await updateIntention(doc, {
      status: result.status === "uncertain" ? "uncertain" : "accepted",
      receipt: result,
      error: result.error,
    });
    return result;
  } catch (error) {
    // A lost response is retried only at the existing idempotent message endpoint.
    if (
      !(error instanceof ApiError) ||
      error.status >= 500 ||
      error.status === 408 ||
      error.status === 429
    ) {
      await updateIntention(doc, { error: errorText(error) });
      return { queued: true, status: "queued" };
    }
    await updateIntention(doc, {
      status: "failed",
      error: errorText(error),
    });
    throw error;
  }
}
function once(doc: any) {
  let task = active.get(doc.id);
  if (!task) {
    task = deliver(doc).finally(() => active.delete(doc.id));
    active.set(doc.id, task);
  }
  return task;
}
export async function durableSend(
  body: Record<string, any>,
  attachments: any[] = [],
) {
  if (typeof body.id !== "string" || !body.id)
    throw new Error("A message identity is required");
  const storage = await syncDatabase().catch((error) => {
    if (error instanceof UnsupportedSyncError) return null;
    throw error;
  });
  if (!storage) return api("/api/messages", body);
  const { db } = storage;
  let doc = await db.outbox.findOne(body.id).exec();
  if (doc) {
    const old: Intention = JSON.parse(doc.payload);
    if (JSON.stringify(old.body) !== JSON.stringify(body))
      throw new Error("Message identity already has different content");
    if (old.status === "failed") throw new Error(old.error || "Message failed");
  } else {
    doc = await db.outbox.insert({
      id: body.id,
      seq: 0,
      payload: JSON.stringify({
        body,
        status: "queued",
        created: Date.now(),
        displayPending: true,
        attachments,
      } satisfies Intention),
    });
  }
  return once(doc);
}
export async function acknowledgeOutbox(ids: string[]) {
  const { db } = await syncDatabase();
  for (const id of ids) {
    const doc = await db.outbox.findOne(id).exec();
    if (doc) await updateIntention(doc, { displayPending: false });
  }
}
export async function editOutboxDisplay(id: string, text: string) {
  const { db } = await syncDatabase();
  const doc = await db.outbox.findOne(id).exec();
  if (doc) await updateIntention(doc, { displayText: text });
}
export function useOutbox() {
  const [entries, setEntries] = useState<OutboxEntry[]>([]);
  const [error, setError] = useState("");
  useEffect(() => {
    let stop = false,
      draining = false;
    let unsubscribe = () => {};
    const drain = async () => {
      if (stop || draining || !navigator.onLine) return;
      draining = true;
      try {
        const { db } = await syncDatabase();
        const docs = await db.outbox.find().exec();
        const queued = docs.filter(
          (doc: any) => JSON.parse(doc.payload).status === "queued",
        );
        if (queued.length) {
          const state = await api("/api/state");
          setToken(state.token);
          for (const doc of queued) {
            if (stop) break;
            await once(doc).catch((e) => {
              // Per-message rejection is already stored beside that message.
              if (!(e instanceof ApiError)) setError(errorText(e));
            });
          }
        }
      } catch (e) {
        if (!stop && !(e instanceof UnsupportedSyncError))
          setError(errorText(e));
      } finally {
        draining = false;
      }
    };
    void syncDatabase()
      .then(({ db }) => {
        if (stop) return;
        const sub = db.outbox.find().$.subscribe((docs: any[]) => {
          setEntries(
            docs
              .map((doc) => ({ id: doc.id, ...JSON.parse(doc.payload) }))
              .sort((a, b) => a.created - b.created),
          );
        });
        unsubscribe = () => sub.unsubscribe();
        void drain();
      })
      .catch((e) => {
        if (!stop && !(e instanceof UnsupportedSyncError))
          setError(errorText(e));
      });
    const timer = setInterval(() => void drain(), 5000);
    window.addEventListener("online", drain);
    return () => {
      stop = true;
      clearInterval(timer);
      unsubscribe();
      window.removeEventListener("online", drain);
    };
  }, []);
  return { entries, error };
}
