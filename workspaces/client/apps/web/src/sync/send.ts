import { useEffect, useState } from "react";
import { outboxRequestKeys, outboxRequests } from "../browserPersistence";
import { serverLocalStorage } from "../servers/storage";
import { ApiError, command, errorText, getStreamSession } from "../api";
import {
  messageReceipt,
  type CommandRequest,
  type MessageDelivery,
  type SendMessageBody,
} from "../commands";
import type { Attachment } from "../components/ComposerAttachments";
import { watchResourceConnection } from "./subscriptions/events";
import { DeliveryStatus } from "../generated/api";
import type { MessageReceipt } from "./messageReceipts";

type SendCommand = Extract<CommandRequest, { kind: "send" }>;
type Intention = {
  body: SendMessageBody;
  status:
    | "queued"
    | "paused"
    | "cancelled"
    | "accepted"
    | "uncertain"
    | "failed";
  attempted?: boolean;
  error?: string | undefined;
  receipt?: MessageDelivery | undefined;
  created: number;
  displayPending?: boolean;
  attachments?: Attachment[] | undefined;
  displayText?: string;
};
export type OutboxEntry = Intention & { id: string };
export type OutgoingMessage = Omit<OutboxEntry, "status"> & {
  status: OutboxEntry["status"] | "sending";
};
type SavedSend = Intention & { request: SendCommand; workspaceId: string };
type SendResult =
  | (MessageDelivery & { kind: "server"; queued: boolean })
  | {
      kind: "local";
      id: string;
      queued: boolean;
      status: Intention["status"];
      error?: string | undefined;
    };

const statuses = new Set([
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
  "stored_only",
]);
export function matchesMessageDelivery(value: unknown, id: string) {
  return (
    !!value &&
    typeof value === "object" &&
    "id" in value &&
    value.id === id &&
    "status" in value &&
    typeof value.status === "string" &&
    statuses.has(value.status)
  );
}

export function createOutbox(options: {
  api: Pick<import("../api").ServerApi, "command" | "getStreamSession">;
  storage: Storage;
  watchResourceConnection: typeof watchResourceConnection;
}) {
  const { command } = options.api;
  const outbox = outboxRequests(options.storage);
  const listeners = new Set<() => void>();
  const active = new Map<string, Promise<SendResult>>();
  const key = (room: string, id: string) => JSON.stringify([room, id]);
  const publish = () => listeners.forEach((listener) => listener());
  function list(): OutboxEntry[] {
    return outboxRequestKeys(options.storage, "messageSend", "")
      .flatMap((suffix) => {
        let identity: unknown;
        try {
          identity = JSON.parse(suffix);
        } catch {
          return [];
        }
        if (
          !Array.isArray(identity) ||
          identity.length !== 2 ||
          !identity.every((value) => typeof value === "string")
        )
          return [];
        const [room, id] = identity as [string, string];
        const value = read(room, id);
        return [
          value
            ? { ...value, id }
            : {
                id,
                body: { id, room, text: "Saved message could not be read." },
                status: "failed" as const,
                error:
                  "This saved message cannot be read. Discard it to remove it.",
                created: 0,
                displayPending: true,
              },
        ];
      })
      .sort((a, b) => a.created - b.created);
  }
  function read(room: string, id: string): SavedSend | undefined {
    const raw = outbox.get("messageSend", key(room, id));
    if (!raw) return;
    try {
      const row: unknown = JSON.parse(raw);
      if (!row || typeof row !== "object") return;
      const value = row as SavedSend;
      if (
        value.body?.id !== id ||
        value.body.room !== room ||
        typeof value.body.text !== "string" ||
        value.request?.id !== id ||
        value.request.kind !== "send" ||
        value.request.target !== room ||
        typeof value.request.client !== "string" ||
        !Number.isSafeInteger(value.request.number) ||
        typeof value.workspaceId !== "string" ||
        typeof value.created !== "number" ||
        !["queued", "paused", "uncertain", "failed"].includes(value.status)
      )
        return;
      const expected = {
        text: value.body.text,
        ...(value.body.assets ? { assets: value.body.assets } : {}),
        ...(value.body.delivery ? { delivery: value.body.delivery } : {}),
      };
      if (JSON.stringify(value.request.body) !== JSON.stringify(expected))
        return;
      return value;
    } catch {
      return;
    }
  }
  function write(value: SavedSend) {
    outbox.set(
      "messageSend",
      key(value.body.room, value.body.id),
      JSON.stringify(value),
    );
    publish();
  }
  function remove(room: string, id: string) {
    outbox.remove("messageSend", key(room, id));
    publish();
  }
  function find(id: string) {
    return list().find((entry) => entry.id === id);
  }
  function result(value: SavedSend): SendResult {
    return {
      kind: "local",
      id: value.body.id,
      queued: value.status === "queued" || value.status === "paused",
      status: value.status,
      error: value.error,
    };
  }
  function send(
    value: SavedSend,
    onPersist?: () => void | Promise<void>,
  ): Promise<SendResult> {
    const id = value.body.id;
    const existing = active.get(id);
    if (existing) return existing;
    const task = (async (): Promise<SendResult> => {
      await onPersist?.();
      try {
        if (!read(value.body.room, id))
          return { kind: "local", id, queued: false, status: "cancelled" };
        const current = options.api.getStreamSession();
        if (!current) throw new Error("The server connection is not ready.");
        if (current.workspaceId !== value.workspaceId)
          throw new ApiError(
            "The server workspace changed. Reload before sending.",
            409,
          );
        write({ ...value, attempted: true });
        const response = await command.run(value.request, {
          workspaceId: value.workspaceId,
        });
        const receipt =
          messageReceipt(response) ??
          (response.status === "applied" &&
          response.ref_kind === "message" &&
          response.ref_id === id
            ? { id, status: DeliveryStatus.pending }
            : undefined);
        if (!receipt || !matchesMessageDelivery(receipt, id))
          throw new Error("The delivery response does not match this message.");
        if (receipt.status === "failed" || receipt.status === "cancelled") {
          const failed = {
            ...value,
            status: "failed" as const,
            attempted: true,
            error: receipt.error || "The message was not sent.",
          };
          if (read(value.body.room, id)) write(failed);
          return result(failed);
        }
        remove(value.body.room, id);
        return {
          ...receipt,
          kind: "server",
          queued: receipt.status === "queued",
        };
      } catch (error) {
        const definitive =
          error instanceof ApiError &&
          error.status < 500 &&
          error.status !== 408 &&
          error.status !== 429;
        const failed = {
          ...value,
          attempted: true,
          status: definitive ? ("failed" as const) : ("uncertain" as const),
          error: errorText(error),
        };
        if (read(value.body.room, id)) write(failed);
        return result(failed);
      }
    })().finally(() => active.delete(id));
    active.set(id, task);
    return task;
  }
  async function durableSend(
    body: SendMessageBody,
    attachments: Attachment[] = [],
    onPersist?: () => void | Promise<void>,
  ) {
    if (!body.id) throw new Error("A message identity is required");
    let value = read(body.room, body.id);
    if (value && JSON.stringify(value.body) !== JSON.stringify(body))
      throw new Error("Message identity already has different content");
    if (!value) {
      if (outbox.get("messageSend", key(body.room, body.id)) !== null)
        throw new Error(
          "This saved message cannot be read. Discard it to remove it.",
        );
      const session = options.api.getStreamSession();
      if (!session) throw new Error("The server connection is not ready.");
      const { workspaceId } = session;
      const request = command.make(
        "send",
        body.room,
        {
          text: body.text,
          ...(body.assets ? { assets: body.assets } : {}),
          ...(body.delivery ? { delivery: body.delivery } : {}),
        },
        body.id,
      );
      value = {
        body,
        request,
        workspaceId,
        status: "queued",
        attempted: false,
        created: Date.now(),
        displayPending: true,
        attachments,
      };
      write(value);
    }
    return send(value, onPersist);
  }
  async function changeOutbox(id: string, action: "cancel" | "resume") {
    const entry = find(id);
    if (!entry) throw new Error("This message is no longer on this device.");
    const value = read(entry.body.room, id);
    if (action === "cancel") {
      remove(entry.body.room, id);
      return;
    }
    if (!value) throw new Error("This saved message cannot be retried.");
    return send(value);
  }
  async function stopRemovedMessage(id: string) {
    const entry = find(id);
    if (!entry) return false;
    const neverAttempted = entry.attempted === false;
    remove(entry.body.room, id);
    return neverAttempted;
  }
  async function acknowledgeOutbox(ids: string[]) {
    for (const id of ids) {
      const entry = find(id);
      if (entry) remove(entry.body.room, id);
    }
  }
  async function reconcileOutboxReceipts(
    room: string,
    receipts: MessageReceipt[],
    workspaceId: string,
    signal?: AbortSignal,
  ) {
    if (signal?.aborted) return;
    for (const receipt of receipts) {
      const value = read(room, receipt.id);
      if (value?.workspaceId === workspaceId) remove(room, receipt.id);
    }
  }
  async function editOutboxDisplay(id: string, text: string) {
    const entry = find(id);
    const value = entry && read(entry.body.room, id);
    if (value) write({ ...value, displayText: text });
  }
  function useOutbox() {
    const [entries, setEntries] = useState<OutboxEntry[]>([]);
    const [error, setError] = useState("");
    useEffect(() => {
      const update = () => {
        try {
          setEntries(list());
          setError("");
        } catch (failure) {
          setError(errorText(failure));
        }
      };
      const retry = () => {
        try {
          for (const entry of list()) {
            const value = read(entry.body.room, entry.id);
            if (
              value &&
              (value.status === "queued" || value.status === "uncertain")
            )
              void send(value).catch((failure) => setError(errorText(failure)));
          }
        } catch (failure) {
          setError(errorText(failure));
        }
      };
      listeners.add(update);
      window.addEventListener("storage", update);
      let wasLive = false;
      const stopConnection = options.watchResourceConnection((status) => {
        const reconnected = status === "live" && !wasLive;
        wasLive = status === "live";
        if (reconnected) retry();
      });
      update();
      return () => {
        listeners.delete(update);
        window.removeEventListener("storage", update);
        stopConnection();
      };
    }, []);
    return { entries, error };
  }
  return {
    durableSend,
    acknowledgeOutbox,
    reconcileOutboxReceipts,
    editOutboxDisplay,
    changeOutbox,
    stopRemovedMessage,
    useOutbox,
  };
}

export const {
  durableSend,
  acknowledgeOutbox,
  reconcileOutboxReceipts,
  editOutboxDisplay,
  changeOutbox,
  stopRemovedMessage,
  useOutbox,
} = createOutbox({
  api: { command, getStreamSession },
  storage: serverLocalStorage,
  watchResourceConnection,
});
