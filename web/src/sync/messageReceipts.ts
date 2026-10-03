import type { Message } from "../types";
import type { OutgoingMessage } from "./send";

export type MessageReceipt = {
  id: string;
  status: string;
  error?: string | null;
};

const statuses = new Set([
  "pending",
  "reserved",
  "dispatching",
  "delivered",
  "uncertain",
  "failed",
  "cancelled",
]);

export function checkedMessageReceipts(
  value: any,
  room: string,
  ids: string[],
): MessageReceipt[] {
  const requested = new Set(ids);
  if (value?.agent !== room || !Array.isArray(value.items))
    throw new Error("The delivery receipts belong to another chat.");
  if (
    value.items.some(
      (item: any) => !requested.has(item?.id) || !statuses.has(item?.status),
    ) ||
    new Set(value.items.map((item: MessageReceipt) => item.id)).size !==
      value.items.length
  )
    throw new Error(
      "The delivery receipts do not match the requested messages.",
    );
  return value.items;
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
