import type { Message } from "../types";
import type { OutgoingMessage } from "../sync/send";

export function explicitQueue(item: Record<string, unknown>) {
  const intent = item.requestedDelivery ?? item.delivery;
  return intent === undefined || intent === "queue";
}

export function dispatchedMessage(item: Message) {
  return (
    item.materialized === true ||
    ["reserved", "dispatching", "delivered", "sent"].includes(
      item.deliveryStatus || "",
    ) ||
    (!item.localDelivery && item.materialized !== false && !item.pending)
  );
}

// Hidden scheduler inputs keep their original slots and the full queue revision.
export function mergeQueueOrder(
  raw: string[],
  visible: string[],
  ordered: string[],
) {
  const selected = new Set(visible);
  if (
    new Set(ordered).size !== selected.size ||
    ordered.length !== visible.length ||
    ordered.some((id) => !selected.has(id)) ||
    visible.some((id) => !raw.includes(id))
  )
    throw new Error("The queue changed. Reload before reordering.");
  let index = 0;
  return raw.map((id) => (selected.has(id) ? ordered[index++] : id));
}

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
      let shown =
        !present.assets?.length &&
        entry.attachments?.length &&
        (present.pending || present.materialized === false)
          ? { ...present, assets: entry.attachments }
          : present;
      if (!shown.requestedDelivery && entry.body.delivery)
        shown = { ...shown, requestedDelivery: entry.body.delivery };
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
          ? ["queued", "pending"].includes(entry.receipt?.status)
            ? "pending"
            : "accepted"
          : entry.status,
      deliveryError: entry.error,
      localDelivery: true,
      requestedDelivery: entry.body.delivery,
    });
  }
  return {
    items: [...items.map((item) => updates.get(item.id) || item), ...extra],
    observed,
  };
}

export function deliveryLabel(item: Message) {
  const status = item.deliveryStatus || (item.pending ? "pending" : "");
  if (status === "pending" && !explicitQueue(item)) return "Sending…";
  return (
    (
      {
        sending: "Sending…",
        reserved: "Starting…",
        dispatching: "Sending…",
        pending: "Queued",
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
