import { serverLocalStorage as localStorage } from "../servers/storage";

import { useEffect, useState } from "react";

import {
  isApiSchemaMismatch,
  onApiSchemaMismatch,
  syncGet,
  syncPost,
  ApiError,
  ApiSchemaMismatchError,
  errorText,
  refreshSession,
} from "../api";

import type { PostBody, PostResult } from "../api";

import { syncDatabase, type SyncDocument } from "./client";

import { onResume } from "./resume";

import {
  checkedMessageReceipts,
  receiptOutgoing,
  type MessageReceipt,
} from "./messageReceipts";

import type { Attachment } from "../components/ComposerAttachments";

type MessageBody = PostBody<"/api/messages">;

type MessageDelivery = PostResult<"/api/messages">;

type StoredMessageReceipt = Pick<MessageDelivery, "id" | "status"> &
  Partial<Pick<MessageDelivery, "deliveries" | "error" | "materialized">>;

const acknowledgedStatuses = {
  queued: true,
  pending: true,
  reserved: true,
  dispatching: true,
  delivered: true,
  accepted: true,
  sent: true,
  uncertain: true,
  failed: true,
  cancelled: true,
  stored_only: true,
} satisfies Record<MessageDelivery["status"], true>;
const AGENT_ENTITY_ID_PREFIX = "entity:agent:";
const OUTBOX_RETRY_DELAY_MS = 5_000;

export function matchesMessageDelivery(value: unknown, messageId: string) {
  if (!value || typeof value !== "object") return false;
  if (!("id" in value) || value.id !== messageId || !("status" in value))
    return false;
  return (
    typeof value.status === "string" &&
    Object.hasOwn(acknowledgedStatuses, value.status)
  );
}

type Intention = {
  body: MessageBody;
  status:
    | "queued"
    | "paused"
    | "cancelled"
    | "accepted"
    | "uncertain"
    | "failed";
  attempted?: boolean;
  error?: string;
  receipt?: StoredMessageReceipt;
  created: number;
  displayPending?: boolean;
  attachments?: Attachment[];
  displayText?: string;
};

export type OutboxEntry = Intention & { id: string };

export type OutgoingMessage = Omit<OutboxEntry, "status"> & {
  status: OutboxEntry["status"] | "sending";
};

type SendResult =
  | (MessageDelivery & { kind: "server"; queued: boolean })
  | {
      kind: "local";
      id: string;
      queued: boolean;
      status: Intention["status"];
      error?: string;
    };

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

export const intentionResult = (value: Intention): SendResult =>
  value.receipt
    ? {
        ...value.receipt,
        ...(value.receipt.deliveries
          ? { deliveries: value.receipt.deliveries }
          : {}),
        kind: "server",
        queued: value.receipt.status === "queued",
      }
    : {
        kind: "local",
        id: value.body.id,
        queued: value.status === "queued" || value.status === "paused",
        status: value.status,
        error: value.error,
      };

export function retainQueuedSchemaMismatch(
  value: Intention,
  error: unknown,
): Intention {
  return { ...value, error: errorText(error) };
}

const removalFence = (workspaceId: string, id: string) =>
  `studio-removed-send:${JSON.stringify([workspaceId, id])}`;
export function createOutbox(options: {
  api: Pick<
    import("../api").ServerApi,
    | "isApiSchemaMismatch"
    | "onApiSchemaMismatch"
    | "syncGet"
    | "syncPost"
    | "refreshSession"
  >;
  syncDatabase: typeof syncDatabase;
  storage: Storage;
}) {
  const {
    isApiSchemaMismatch,
    onApiSchemaMismatch,
    syncGet,
    syncPost,
    refreshSession,
  } = options.api;
  const { syncDatabase } = options;
  const localStorage = options.storage;

  const active = new Map<string, Promise<SendResult>>();

  const sendingRooms = new Set<string>();

  async function deliver(doc: any): Promise<SendResult> {
    let value: Intention = JSON.parse(doc.getLatest().payload);
    if (value.status !== "queued" || !navigator.onLine)
      return intentionResult(value);
    try {
      const { workspaceId } = await syncDatabase();
      const [identity, session] = await Promise.all([
        syncGet("/api/sync/identity"),
        refreshSession(),
      ]);
      if (identity.workspaceId !== workspaceId)
        throw new ApiError(
          "The server workspace changed. Reload before sending.",
          409,
        );
      // Claim the current content before HTTP. Cancellation uses the same atomic
      // update and can succeed only before this request starts.
      const claimed = await doc.incrementalModify((record: any) => {
        const current: Intention = JSON.parse(record.payload);
        if (
          current.status === "queued" &&
          localStorage.getItem(removalFence(workspaceId, doc.id)) === "1"
        ) {
          const cancelled = current.attempted === false;
          return {
            ...record,
            payload: JSON.stringify({
              ...current,
              status: cancelled ? "cancelled" : "paused",
              ...(cancelled
                ? { receipt: { id: doc.id, status: "cancelled" } }
                : {}),
            }),
          };
        }
        return current.status === "queued"
          ? {
              ...record,
              payload: JSON.stringify({ ...current, attempted: true }),
            }
          : record;
      });
      value = JSON.parse(claimed.payload);
      if (value.status !== "queued") return intentionResult(value);
      const result = await syncPost("/api/messages", value.body, {
        workspaceId,
        sessionToken: session.token,
      });
      if (!matchesMessageDelivery(result, value.body.id))
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
        const confirmed = (receipt: StoredMessageReceipt | undefined) =>
          receipt?.status === "delivered" ||
          receipt?.status === "accepted" ||
          receipt?.status === "sent";
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
      if (error instanceof ApiSchemaMismatchError) {
        const current = await doc.incrementalModify((record: any) => {
          const stored: Intention = JSON.parse(record.payload);
          if (stored.status !== "queued") return record;
          return {
            ...record,
            payload: JSON.stringify(retainQueuedSchemaMismatch(stored, error)),
          };
        });
        return intentionResult(JSON.parse(current.payload));
      }
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

  function once(doc: any): Promise<SendResult> {
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

  async function durableSend(
    body: MessageBody,
    attachments: Attachment[] = [],
    onPersist?: () => void | Promise<void>,
  ) {
    if (typeof body.id !== "string" || !body.id)
      throw new Error("A message identity is required");
    const { db } = await syncDatabase();
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
    await onPersist?.();
    return once(doc);
  }

  async function acknowledgeOutbox(ids: string[]) {
    const { db } = await syncDatabase();
    for (const id of ids) {
      const doc = await db.outbox.findOne(id).exec();
      if (doc) await updateIntention(doc, { displayPending: false });
    }
  }

  async function reconcileOutboxReceipts(
    room: string,
    receipts: MessageReceipt[],
    workspaceId: string,
    signal?: AbortSignal,
  ) {
    if (signal?.aborted) return;
    const current = await syncDatabase();
    if (signal?.aborted) return;
    if (current.workspaceId !== workspaceId) return;
    await current.verifyWorkspace();
    if (signal?.aborted) return;
    for (const receipt of receipts) {
      if (signal?.aborted) return;
      const doc = await current.db.outbox.findOne(receipt.id).exec();
      if (signal?.aborted) return;
      if (!doc) continue;
      await doc.incrementalModify((record: any) => {
        if (signal?.aborted) return record;
        const value = JSON.parse(record.payload);
        if (value.body.room !== room) return record;
        const next = receiptOutgoing(value, receipt);
        const payload = JSON.stringify(
          next.status === "accepted" &&
            next.receipt?.status === "delivered" &&
            next.receipt.materialized === true
            ? { ...next, displayPending: false }
            : next,
        );
        return payload === record.payload ? record : { ...record, payload };
      });
    }
  }
  function confirmedDeletedAgentProjection(value: unknown, room: string) {
    if (!value || typeof value !== "object") return false;
    const document = value as {
      id?: unknown;
      seq?: unknown;
      _deleted?: unknown;
      entity?: { collection?: unknown; id?: unknown; value?: unknown };
    };
    if (
      document.id !== `${AGENT_ENTITY_ID_PREFIX}${room}` ||
      !Number.isSafeInteger(document.seq) ||
      (document.seq as number) <= 0
    )
      return false;
    if (document._deleted === true) return true;
    const record = document.entity;
    if (
      record?.collection !== "agent" ||
      record.id !== room ||
      !record.value ||
      typeof record.value !== "object"
    )
      return false;
    const agent = record.value as { id?: unknown; deletedAt?: unknown };
    return agent.id === room && Boolean(agent.deletedAt);
  }

  async function recoverAcknowledgedOutbox() {
    const { db, workspaceId, verifyWorkspace } = await syncDatabase();
    await verifyWorkspace();
    const rooms = new Map<string, string[]>();
    for (const doc of await db.outbox.find().exec()) {
      const value: Intention = JSON.parse(doc.getLatest().payload);
      if (
        value.displayPending === false ||
        !["accepted", "uncertain"].includes(value.status)
      )
        continue;
      const ids = rooms.get(value.body.room) || [];
      ids.push(doc.id);
      rooms.set(value.body.room, ids);
    }
    const roomBatches = [...rooms].map(([room, ids]) => ({
      room,
      batches: Array.from({ length: Math.ceil(ids.length / 100) }, (_, index) =>
        ids.slice(index * 100, (index + 1) * 100),
      ),
    }));
    const batchCount = roomBatches.reduce(
      (count, item) => count + item.batches.length,
      0,
    );
    if (!batchCount) return;

    const deletedRooms = new Set<string>();
    const controllers = new Map(
      [...rooms.keys()].map((room) => [room, new AbortController()] as const),
    );
    const markDeleted = (document: unknown) => {
      if (!document || typeof document !== "object") return;
      const id = (document as { id?: unknown }).id;
      if (typeof id !== "string" || !id.startsWith(AGENT_ENTITY_ID_PREFIX))
        return;
      const room = id.slice(AGENT_ENTITY_ID_PREFIX.length);
      const controller = controllers.get(room);
      if (!controller || !confirmedDeletedAgentProjection(document, room))
        return;
      deletedRooms.add(room);
      controller.abort();
    };
    const projectionSubscription = db.projections.$.subscribe((event: any) =>
      markDeleted(event.documentData),
    );
    let nextRoom = 0;
    let failure: unknown;
    try {
      const currentAgents =
        await db.projections.storageInstance.findDocumentsById(
          [...rooms.keys()].map((room) => `${AGENT_ENTITY_ID_PREFIX}${room}`),
          true,
        );
      for (const document of currentAgents) markDeleted(document);

      await Promise.all(
        Array.from({ length: Math.min(4, roomBatches.length) }, async () => {
          while (nextRoom < roomBatches.length) {
            const { room, batches } = roomBatches[nextRoom++];
            const controller = controllers.get(room)!;
            for (const ids of batches) {
              if (deletedRooms.has(room) || controller.signal.aborted) break;
              try {
                const response = await syncGet("/api/messages/receipts", {
                  query: { agent: room, ids: JSON.stringify(ids) },
                  workspaceId,
                  signal: controller.signal,
                });
                if (deletedRooms.has(room) || controller.signal.aborted) break;
                await reconcileOutboxReceipts(
                  room,
                  checkedMessageReceipts(response, room, ids, workspaceId),
                  workspaceId,
                  controller.signal,
                );
              } catch (error) {
                if (!controller.signal.aborted) failure ??= error;
              }
            }
          }
        }),
      );
    } finally {
      projectionSubscription.unsubscribe();
    }
    if (failure) throw failure;
  }
  async function editOutboxDisplay(id: string, text: string) {
    const { db } = await syncDatabase();
    const doc = await db.outbox.findOne(id).exec();
    if (doc) await updateIntention(doc, { displayText: text });
  }

  async function changeOutbox(
    id: string,
    action: "cancel" | "pause" | "resume",
  ) {
    const { db, workspaceId } = await syncDatabase();
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
      } else {
        if (action === "resume")
          localStorage.removeItem(removalFence(workspaceId, id));
        next.status = action === "pause" ? "paused" : "queued";
      }
      return { ...record, payload: JSON.stringify(next) };
    });
    if (action === "resume") void once(updated).catch(() => {});
  }

  async function stopRemovedMessage(id: string) {
    const { db, workspaceId } = await syncDatabase();
    // The card can be removed before durableSend has inserted its document.
    // The HTTP claim checks this fence, including claims from another tab.
    localStorage.setItem(removalFence(workspaceId, id), "1");
    const doc = await db.outbox.findOne(id).exec();
    if (!doc) return false;
    const updated = await doc.incrementalModify((record: any) => {
      const current: Intention = JSON.parse(record.payload);
      if (!["queued", "paused"].includes(current.status)) return record;
      // A claimed request can still finish. Keep its identity and receipt.
      const cancelled = current.attempted === false;
      return {
        ...record,
        payload: JSON.stringify({
          ...current,
          status: cancelled ? "cancelled" : "paused",
          ...(cancelled ? { receipt: { id, status: "cancelled" } } : {}),
        }),
      };
    });
    return JSON.parse(updated.payload).status === "cancelled";
  }

  function useOutbox() {
    const [entries, setEntries] = useState<OutboxEntry[]>([]);
    const [error, setError] = useState("");
    useEffect(() => {
      let stop = false,
        draining = false;
      let unsubscribe = () => {};
      let queuedIdentity = "";
      let drainAgain = false;
      let retryTimer: ReturnType<typeof setTimeout> | undefined;
      const clearRetry = () => {
        if (retryTimer === undefined) return;
        clearTimeout(retryTimer);
        retryTimer = undefined;
      };
      const scheduleRetry = () => {
        if (
          retryTimer !== undefined ||
          stop ||
          isApiSchemaMismatch() ||
          !navigator.onLine
        )
          return;
        retryTimer = setTimeout(() => {
          retryTimer = undefined;
          if (stop || isApiSchemaMismatch() || !navigator.onLine) return;
          if (subscribed) void drain();
          else void subscribe();
        }, OUTBOX_RETRY_DELAY_MS);
      };
      const drain = async () => {
        if (stop || isApiSchemaMismatch() || !navigator.onLine) return;
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
            while (!stop && !isApiSchemaMismatch() && nextRoom < rooms.length) {
              const room = rooms[nextRoom++];
              while (!stop && !isApiSchemaMismatch()) {
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
                if (
                  result?.kind === "local" &&
                  result.status === "queued" &&
                  result.error
                )
                  scheduleRetry();
                if (result?.queued) break;
              }
            }
          };
          // Bound reconnect traffic while letting another chat pass a stalled one.
          await Promise.all(
            Array.from({ length: Math.min(4, rooms.length) }, worker),
          );
          if (!stop && !isApiSchemaMismatch()) setError("");
        } catch (e) {
          if (!stop) {
            setError(errorText(e));
            scheduleRetry();
          }
        } finally {
          draining = false;
          if (
            drainAgain &&
            retryTimer === undefined &&
            !stop &&
            !isApiSchemaMismatch()
          )
            void drain();
        }
      };
      let subscribing = false,
        subscribed = false;
      let recoveringReceipts = false;
      const recoverReceipts = () => {
        if (stop || recoveringReceipts || isApiSchemaMismatch()) return;
        recoveringReceipts = true;
        void recoverAcknowledgedOutbox()
          .catch(() => {})
          .finally(() => {
            recoveringReceipts = false;
          });
      };
      const handleOutboxDocuments = (docs: any[]) => {
        const nextQueued = JSON.stringify(
          queuedDocuments(docs).map((doc) => doc.id),
        );
        const queuedChanged = nextQueued !== queuedIdentity;
        if (queuedChanged) {
          queuedIdentity = nextQueued;
          clearRetry();
          void drain();
        }
        setEntries(
          docs
            .map((doc) => ({ id: doc.id, ...JSON.parse(doc.payload) }))
            .sort((a, b) => a.created - b.created),
        );
        return queuedChanged;
      };
      const subscribe = async () => {
        if (stop || subscribing || subscribed) return;
        subscribing = true;
        try {
          const { db, workspaceId } = await syncDatabase();
          if (stop) return;
          let subscriptionReady = false;
          let bufferedDocs: any[] | undefined;
          const sub = db.outbox.find().$.subscribe(
            (docs: any[]) => {
              if (!subscriptionReady) bufferedDocs = docs;
              else handleOutboxDocuments(docs);
            },
            (error: unknown) => {
              subscribed = false;
              unsubscribe();
              unsubscribe = () => {};
              if (!stop) {
                setError(errorText(error));
                scheduleRetry();
              }
            },
          );
          unsubscribe = () => sub.unsubscribe();
          if (sub.closed) {
            unsubscribe();
            unsubscribe = () => {};
            subscribed = false;
            scheduleRetry();
            return;
          }
          const receiptController = new AbortController();
          let receiptQueue = Promise.resolve();
          const applyReceipt = (document: SyncDocument) => {
            const entity = document.entity;
            if (
              document._deleted ||
              entity?.collection !== "messageReceipt" ||
              document.id !== `entity:messageReceipt:${entity.id}` ||
              typeof entity.value.agent !== "string" ||
              !matchesMessageDelivery(entity.value, entity.id)
            )
              return;
            const room = entity.value.agent;
            const receipt = entity.value;
            receiptQueue = receiptQueue
              .then(async () => {
                if (!stop && !receiptController.signal.aborted)
                  await reconcileOutboxReceipts(
                    room,
                    [receipt],
                    workspaceId,
                    receiptController.signal,
                  );
              })
              .catch((error: unknown) => {
                if (!stop) setError(errorText(error));
              });
          };
          const receiptSubscription = db.projections.$.subscribe((event) =>
            applyReceipt(event.documentData),
          );
          unsubscribe = () => {
            sub.unsubscribe();
            receiptSubscription.unsubscribe();
            receiptController.abort();
          };
          subscribed = true;
          clearRetry();
          subscriptionReady = true;
          if (bufferedDocs) handleOutboxDocuments(bufferedDocs);
          void drain();
          recoverReceipts();
          // A hidden accepted row still owns delivery state. Read cached receipts
          // once, then apply their persisted stream changes without another GET.
          const outbox = bufferedDocs ?? (await db.outbox.find().exec());
          const ids = outbox
            .filter((doc: any) =>
              ["accepted", "uncertain"].includes(
                JSON.parse(doc.getLatest().payload).status,
              ),
            )
            .map((doc: any) => `entity:messageReceipt:${doc.id}`);
          if (ids.length) {
            const receipts =
              await db.projections.storageInstance.findDocumentsById(ids, true);
            for (const receipt of receipts) applyReceipt(receipt);
          }
        } catch (error) {
          if (!stop) {
            setError(errorText(error));
            scheduleRetry();
          }
        } finally {
          subscribing = false;
        }
      };
      const resume = () => {
        clearRetry();
        void subscribe();
        void drain();
      };
      void subscribe();
      const stopForSchemaMismatch = onApiSchemaMismatch(() => {
        clearRetry();
      });
      const stopResume = onResume(() => {
        resume();
        recoverReceipts();
      });
      return () => {
        stop = true;
        clearRetry();
        unsubscribe();
        stopResume();
        stopForSchemaMismatch();
      };
    }, []);
    return { entries, error };
  }
  return {
    deliver,
    durableSend,
    acknowledgeOutbox,
    reconcileOutboxReceipts,
    recoverAcknowledgedOutbox,
    editOutboxDisplay,
    changeOutbox,
    stopRemovedMessage,
    useOutbox,
  };
}
export const {
  deliver,
  durableSend,
  acknowledgeOutbox,
  reconcileOutboxReceipts,
  recoverAcknowledgedOutbox,
  editOutboxDisplay,
  changeOutbox,
  stopRemovedMessage,
  useOutbox,
} = createOutbox({
  api: {
    isApiSchemaMismatch,
    onApiSchemaMismatch,
    syncGet,
    syncPost,
    refreshSession,
  },
  syncDatabase,
  storage: localStorage,
});
