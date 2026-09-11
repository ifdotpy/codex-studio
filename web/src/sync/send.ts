import { useEffect, useState } from "react";
import { syncApi, ApiError, errorText, setToken } from "../api";
import { syncDatabase, UnsupportedSyncError } from "./client";
import { onResume } from "./resume";

type Intention = {
  body: Record<string, any>;
  status:
    | "queued"
    | "paused"
    | "cancelled"
    | "accepted"
    | "uncertain"
    | "failed";
  attempted?: boolean;
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
const sendingRooms = new Set<string>();
const queuedDocuments = (docs: any[]) =>
  docs
    .filter((doc) => JSON.parse(doc.getLatest().payload).status === "queued")
    .sort(
      (a, b) =>
        JSON.parse(a.payload).created - JSON.parse(b.payload).created ||
        a.id.localeCompare(b.id),
    );
const updateIntention = (doc: any, change: Partial<Intention>) =>
  doc.incrementalModify((record: any) => ({
    ...record,
    payload: JSON.stringify({ ...JSON.parse(record.payload), ...change }),
  }));
const intentionResult = (value: Intention) =>
  value.receipt || {
    queued: value.status === "queued" || value.status === "paused",
    status: value.status,
    error: value.error,
  };
async function deliver(doc: any) {
  let value: Intention = JSON.parse(doc.getLatest().payload);
  if (value.status !== "queued" || !navigator.onLine)
    return intentionResult(value);
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
    // Claim the current content before HTTP. Cancellation uses the same atomic
    // update and can succeed only before this request starts.
    const claimed = await doc.incrementalModify((record: any) => {
      const current: Intention = JSON.parse(record.payload);
      return current.status === "queued"
        ? {
            ...record,
            payload: JSON.stringify({ ...current, attempted: true }),
          }
        : record;
    });
    value = JSON.parse(claimed.payload);
    if (value.status !== "queued") return intentionResult(value);
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
    const current = await doc.incrementalModify((record: any) => {
      const stored: Intention = JSON.parse(record.payload);
      // Preserve confirmed delivery against an older queue receipt. A matching
      // later reply can still resolve queue acceptance or an uncertain outcome.
      const confirmed = (receipt: any) =>
        ["delivered", "accepted", "sent"].includes(receipt?.status);
      if (
        stored.status === "cancelled" ||
        (["accepted", "uncertain"].includes(stored.status) &&
          (confirmed(stored.receipt) ||
            (!confirmed(result) && result.status !== "uncertain")))
      )
        return record;
      return {
        ...record,
        payload: JSON.stringify({
          ...stored,
          status: result.status === "uncertain" ? "uncertain" : "accepted",
          receipt: result,
          error: result.error,
        }),
      };
    });
    return intentionResult(JSON.parse(current.payload));
  } catch (error) {
    // A lost response is retried only at the existing idempotent message endpoint.
    if (
      !(error instanceof ApiError) ||
      error.status >= 500 ||
      error.status === 408 ||
      error.status === 429
    ) {
      const current = await doc.incrementalModify((record: any) => {
        const stored: Intention = JSON.parse(record.payload);
        if (!["queued", "paused"].includes(stored.status)) return record;
        return {
          ...record,
          payload: JSON.stringify({ ...stored, error: errorText(error) }),
        };
      });
      return intentionResult(JSON.parse(current.payload));
    }
    const current = await doc.incrementalModify((record: any) => {
      const stored: Intention = JSON.parse(record.payload);
      if (!["queued", "paused"].includes(stored.status)) return record;
      return {
        ...record,
        payload: JSON.stringify({
          ...stored,
          status: "failed",
          error: errorText(error),
        }),
      };
    });
    const stored: Intention = JSON.parse(current.payload);
    if (stored.status !== "failed") return intentionResult(stored);
    throw error;
  }
}
function once(doc: any) {
  let task = active.get(doc.id);
  if (!task) {
    task = (async () => {
      const { db, workspaceId } = await syncDatabase();
      const value: Intention = JSON.parse(doc.getLatest().payload);
      if (value.status !== "queued" || !navigator.onLine)
        return intentionResult(value);
      const room = `${workspaceId}:${JSON.stringify(value.body.room)}`;
      if (sendingRooms.has(room)) return intentionResult(value);
      const first = queuedDocuments(await db.outbox.find().exec()).find(
        (queued) => JSON.parse(queued.payload).body.room === value.body.room,
      );
      if (first && first.id !== doc.id) return intentionResult(value);
      // Serialize each chat on this device, including browsers without Web Locks.
      if (sendingRooms.has(room)) return intentionResult(value);
      sendingRooms.add(room);
      try {
        // Another tab can retry the same oldest request while this tab is
        // suspended. The server owns deduplication by immutable message ID.
        return await deliver(doc);
      } finally {
        sendingRooms.delete(room);
      }
    })().finally(() => active.delete(doc.id));
    active.set(doc.id, task);
  }
  return task;
}
export async function durableSend(
  body: Record<string, any>,
  attachments: any[] = [],
  onPersist?: () => void,
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
          attempted: false,
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
  if (old.status === "failed")
    throw new ApiError(old.error || "Message failed", 400);
  onPersist?.();
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
export async function changeOutbox(
  id: string,
  action: "cancel" | "pause" | "resume",
) {
  const { db } = await syncDatabase();
  const doc = await db.outbox.findOne(id).exec();
  if (!doc) throw new Error("This message is no longer on this device.");
  const updated = await doc.incrementalModify((record: any) => {
    const current: Intention = JSON.parse(record.payload);
    if (!["queued", "paused"].includes(current.status))
      throw new Error("This message already left the device queue.");
    const next = { ...current };
    if (action === "cancel") {
      // Older records lack this marker and may already have reached the server.
      if (current.attempted !== false)
        throw new Error(
          "Delivery may have started. Stop retrying or check the conversation.",
        );
      next.status = "cancelled";
      next.error = undefined;
      next.receipt = { id, status: "cancelled" };
    } else next.status = action === "pause" ? "paused" : "queued";
    return { ...record, payload: JSON.stringify(next) };
  });
  if (action === "resume") void once(updated).catch(() => {});
}
export function useOutbox() {
  const [entries, setEntries] = useState<OutboxEntry[]>([]);
  const [error, setError] = useState("");
  useEffect(() => {
    let stop = false,
      draining = false;
    let unsubscribe = () => {};
    let queuedIdentity = "";
    let drainAgain = false;
    const drain = async () => {
      if (stop || !navigator.onLine) return;
      if (draining) {
        drainAgain = true;
        return;
      }
      draining = true;
      drainAgain = false;
      try {
        const { db } = await syncDatabase();
        const rooms = [
          ...new Set<string>(
            queuedDocuments(await db.outbox.find().exec()).map(
              (doc) => JSON.parse(doc.payload).body.room,
            ),
          ),
        ];
        let nextRoom = 0;
        const worker = async () => {
          while (!stop && nextRoom < rooms.length) {
            const room = rooms[nextRoom++];
            while (!stop) {
              // Re-read after acceptance so a message saved during HTTP does
              // not wait for the polling timer. Preserve order within each chat.
              const doc = queuedDocuments(await db.outbox.find().exec()).find(
                (queued) => JSON.parse(queued.payload).body.room === room,
              );
              if (!doc) break;
              const result = await once(doc).catch((e) => {
                // Per-message rejection is already stored beside that message.
                if (!(e instanceof ApiError)) throw e;
              });
              if (result?.queued) break;
            }
          }
        };
        // Bound reconnect traffic while letting another chat pass a stalled one.
        await Promise.all(
          Array.from({ length: Math.min(4, rooms.length) }, worker),
        );
        if (!stop) setError("");
      } catch (e) {
        if (!stop && !(e instanceof UnsupportedSyncError))
          setError(errorText(e));
      } finally {
        draining = false;
        if (drainAgain && !stop) void drain();
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
          const nextQueued = JSON.stringify(
            queuedDocuments(docs).map((doc) => doc.id),
          );
          if (nextQueued !== queuedIdentity) {
            queuedIdentity = nextQueued;
            void drain();
          }
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
