import type { GetResult } from "../api";
import type { Message } from "../types";
import type { OutgoingMessage } from "./send";

type MessageReceiptResponse = GetResult<"/api/messages/receipts">;
export type MessageReceipt = MessageReceiptResponse["items"][number];

const statuses = {
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
} satisfies Record<MessageReceipt["status"], true>;

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object";
}

function isMessageReceipt(value: unknown): value is MessageReceipt {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.status === "string" &&
    Object.hasOwn(statuses, value.status) &&
    (value.error === undefined ||
      value.error === null ||
      typeof value.error === "string")
  );
}

export function checkedMessageReceipts(
  value: unknown,
  room: string,
  ids: string[],
): MessageReceipt[] {
  const requested = new Set(ids);
  if (!isRecord(value) || value.agent !== room || !Array.isArray(value.items))
    throw new Error("The delivery receipts belong to another chat.");
  const receipts: MessageReceipt[] = [];
  for (const item of value.items) {
    if (!isMessageReceipt(item) || !requested.has(item.id))
      throw new Error(
        "The delivery receipts do not match the requested messages.",
      );
    receipts.push(item);
  }
  if (new Set(receipts.map((item) => item.id)).size !== receipts.length)
    throw new Error(
      "The delivery receipts do not match the requested messages.",
    );
  return receipts;
}

export function latestMessageReceipt(
  previous: MessageReceipt | undefined,
  next: MessageReceipt,
) {
  return previous?.status === "delivered" && next.status !== "delivered"
    ? previous
    : next;
}

export function receiptOutgoing(
  entry: OutgoingMessage,
  receipt?: MessageReceipt,
): OutgoingMessage {
  // Only reconcile requests the server has already acknowledged. An unknown
  // HTTP outcome still uses the original idempotent send path.
  if (!receipt || !["accepted", "uncertain"].includes(entry.status))
    return entry;
  const current = latestMessageReceipt(entry.receipt, receipt);
  return {
    ...entry,
    status:
      current.status === "uncertain"
        ? "uncertain"
        : current.status === "failed"
          ? "failed"
          : current.status === "cancelled"
            ? "cancelled"
            : "accepted",
    receipt: current,
    error: current.error || undefined,
  };
}

export function receiptTranscript(
  items: Message[],
  room: string,
  receipts: Map<string, MessageReceipt>,
) {
  return items.map((item) => {
    if (item.role !== "user") return item;
    const id =
      item.clientMessageId ||
      (item.id.startsWith(room + ":")
        ? item.id.slice(room.length + 1)
        : item.id);
    const receipt = receipts.get(id);
    if (!receipt) return item;
    const status =
      item.deliveryStatus === "delivered" ? "delivered" : receipt.status;
    return {
      ...item,
      deliveryStatus: status,
      pending: status === "pending",
      deliveryError:
        status === "delivered" ? undefined : receipt.error || undefined,
    };
  });
}
