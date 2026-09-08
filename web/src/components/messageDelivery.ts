import type { Message } from "../types";
import type { OutgoingMessage } from "../sync/send";

export const messageRenderKey = (item: Message) =>
  item.role === "user" && item.clientMessageId
    ? `client:${item.clientMessageId}`
    : item.id;

export function receiptMessage(item: Message, entry: OutgoingMessage) {
  const key = `${entry.body.room}:${entry.id}`;
  return (
    item.clientMessageId === entry.id ||
    item.id === entry.id ||
    item.id === key ||
    (!item.clientMessageId && item.id === `${key}:0`)
  );
}

export function outgoingTranscript(
  items: Message[],
  entries: OutgoingMessage[],
) {
  const observed: string[] = [];
  const extra: Message[] = [];
  const updates = new Map<string, Message>();
  for (const entry of entries) {
    const present = items.find((item) => receiptMessage(item, entry));
    if (present) {
      const shown =
        !present.assets?.length &&
        entry.attachments?.length &&
        (present.pending || present.materialized === false)
          ? { ...present, assets: entry.attachments }
          : present;
      if (shown !== present) updates.set(present.id, shown);
      if (
        ["failed", "uncertain"].includes(entry.status) &&
        !present.deliveryStatus
      )
        updates.set(present.id, {
          ...shown,
          deliveryStatus: entry.status,
          deliveryError: entry.error,
        });
      // Pending queue rows can disappear from older servers during dispatch.
      // Keep the local receipt until a durable transcript record replaces it.
      if (
        !present.pending &&
        present.materialized !== false &&
        (!["failed", "uncertain"].includes(entry.status) ||
          present.deliveryStatus)
      )
        observed.push(entry.id);
      continue;
    }
    extra.push({
      id: `${entry.body.room}:${entry.id}`,
      clientMessageId: entry.id,
      role: "user",
      title: "You",
      text: entry.displayText ?? entry.body.text,
      assets: entry.attachments || [],
      at: entry.created / 1000,
      deliveryStatus:
        entry.status === "accepted"
          ? entry.receipt?.status === "queued"
            ? "pending"
            : "accepted"
          : entry.status,
      deliveryError: entry.error,
      localDelivery: true,
    });
  }
  return {
    items: [...items.map((item) => updates.get(item.id) || item), ...extra],
    observed,
  };
}

export function deliveryLabel(item: Message) {
  const status = item.deliveryStatus || (item.pending ? "pending" : "");
  return (
    (
      {
        sending: "Sending…",
        reserved: "Starting…",
        dispatching: "Sending…",
        pending: "Queued · after turn",
        queued: "Waiting to send",
        paused: "Retries paused",
        uncertain: "Delivery unconfirmed",
        failed: "Not sent",
        accepted: "Sent",
        cancelled: "Cancelled",
      } as Record<string, string>
    )[status] || ""
  );
}
