import { useEffect, useState } from "react";
import { syncApi, ApiError, errorText, setToken } from "../api";
import { syncDatabase, UnsupportedSyncError } from "./client";
import { onResume } from "./resume";

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
    const identity = await syncApi<{ workspaceId: string }>(
      "/api/sync/identity",
    );
    if (identity.workspaceId !== workspaceId)
      throw new ApiError(
        "The server workspace changed. Reload before sending.",
        409,
      );
    let session;
    try {
      session = await syncApi<{ token: string }>("/api/session");
    } catch (error) {
      if (!(error instanceof ApiError && error.status === 404)) throw error;
      session = await syncApi<{ token: string }>("/api/state");
    }
    setToken(session.token);
    const result = await syncApi<any>("/api/messages", value.body);
    const acknowledged = [
      "queued",
      "pending",
      "reserved",
      "dispatching",
      "delivered",
      "accepted",
      "sent",
      "uncertain",
      "failed",
      "cancelled",
    ].includes(result?.status);
    const legacyReceipt =
      result?.status === undefined &&
      result?.room === value.body.room &&
      result?.text === value.body.text?.trim() &&
      result?.deliveries &&
      typeof result.deliveries === "object";
    if (result?.id !== value.body.id || (!acknowledged && !legacyReceipt))
      throw new Error("The delivery response does not match this message.");
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
  if (!storage) return syncApi("/api/messages", body);
  const { db } = storage;
  let doc = await db.outbox.findOne(body.id).exec();
  if (!doc) {
    try {
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
    } catch (error) {
      doc = await db.outbox.findOne(body.id).exec();
      if (!doc) throw error;
    }
  }
  const old: Intention = JSON.parse(doc.payload);
  if (JSON.stringify(old.body) !== JSON.stringify(body))
    throw new Error("Message identity already has different content");
  if (old.status === "failed") throw new Error(old.error || "Message failed");
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
        const queued = docs
          .filter((doc: any) => JSON.parse(doc.payload).status === "queued")
          .sort(
            (a: any, b: any) =>
              JSON.parse(a.payload).created - JSON.parse(b.payload).created ||
              a.id.localeCompare(b.id),
          );
        if (queued.length) {
          for (const doc of queued) {
            if (stop) break;
            const result = await once(doc).catch((e) => {
              // Per-message rejection is already stored beside that message.
              if (!(e instanceof ApiError)) throw e;
            });
            if (result?.queued) break;
          }
        }
        if (!stop) setError("");
      } catch (e) {
        if (!stop && !(e instanceof UnsupportedSyncError))
          setError(errorText(e));
      } finally {
        draining = false;
      }
    };
    let subscribing = false,
      subscribed = false;
    const subscribe = async () => {
      if (stop || subscribing || subscribed) return;
      subscribing = true;
      try {
        const { db } = await syncDatabase();
        if (stop) return;
        const sub = db.outbox.find().$.subscribe((docs: any[]) => {
          setEntries(
            docs
              .map((doc) => ({ id: doc.id, ...JSON.parse(doc.payload) }))
              .sort((a, b) => a.created - b.created),
          );
        });
        unsubscribe = () => sub.unsubscribe();
        subscribed = true;
        void drain();
      } catch (error) {
        if (!stop && !(error instanceof UnsupportedSyncError))
          setError(errorText(error));
      } finally {
        subscribing = false;
      }
    };
    const resume = () => {
      void subscribe();
      void drain();
    };
    void subscribe();
    const timer = setInterval(resume, 5000);
    const stopResume = onResume(resume);
    return () => {
      stop = true;
      clearInterval(timer);
      unsubscribe();
      stopResume();
    };
  }, []);
  return { entries, error };
}
