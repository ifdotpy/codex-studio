import { api, syncApi, ApiError } from "../api";
import type { Message, Snapshot } from "../types";
import { transcriptMessages } from "../hooks";
import { stopRemovedMessage, type OutgoingMessage } from "../sync/send";
import {
  isRemovedMessage,
  removeMessageFromDevice,
  saveRemovedMessage,
} from "./removedMessages";
import type { QueueItem } from "./MessageQueue";

export const isSendingMessage = (message: Message) =>
  message.role === "user" &&
  ["sending", "reserved", "dispatching"].includes(message.deliveryStatus || "");

type Queue = {
  items: QueueItem[];
  revision: string;
  capabilities?: { receipts?: boolean };
};
type Target = { chat: string; message: Message; queue?: Queue; kind?: string };
type Scope = { stateDir: string; workspaceId?: string; kind?: string };

export async function removeSendingMessage(scope: Scope, target: Target) {
  const { chat, message } = target;
  const kind = target.kind || scope.kind || "agent";
  if (!isSendingMessage(message)) return;
  // Preserve a recoverable copy before cancellation can remove a queue receipt.
  saveRemovedMessage(scope.stateDir, kind, chat, message);
  const clientId = message.clientMessageId;
  let cancelled = clientId ? await stopRemovedMessage(clientId) : false;
  if (kind === "agent" && clientId && message.deliveryStatus === "sending") {
    const key = `studio-remove-sending:${JSON.stringify([scope.stateDir, scope.workspaceId, chat, clientId])}`;
    const saved = localStorage.getItem(key);
    let request = saved ? JSON.parse(saved) : null;
    if (!request) {
      const queue =
        target.queue ||
        (await api<Queue>(
          `/api/queue?agent=${encodeURIComponent(chat)}`,
          undefined,
          { workspaceId: scope.workspaceId },
        ));
      const row = queue.items.find((item) => item.id === clientId);
      // Internal follow-ups continue. Only user input can be cancelled here.
      if (
        row?.kind === "user" &&
        queue.revision &&
        queue.capabilities?.receipts
      ) {
        request = {
          action: "cancel",
          agent: chat,
          id: clientId,
          expectedText: row.text,
          expected_revision: queue.revision,
          request_id: crypto.randomUUID(),
        };
        localStorage.setItem(key, JSON.stringify(request));
      }
    }
    if (request) {
      try {
        await syncApi("/api/queue", request, {
          workspaceId: scope.workspaceId,
        });
        cancelled = true;
      } catch (error) {
        // Never repeat an unknown mutation with a new identity. A race with
        // native dispatch cannot turn removal into an agent interruption.
        if (!(error instanceof ApiError) || error.status !== 400) throw error;
      }
    }
  }
  removeMessageFromDevice(scope.stateDir, kind, chat, {
    ...message,
    deliveryStatus: cancelled ? "cancelled" : "uncertain",
  });
}

export async function removeAllSendingMessages(
  scope: Scope,
  data: Snapshot,
  outgoing: OutgoingMessage[],
) {
  const targets = new Map<string, Target>();
  const include = (target: Target) => {
    if (
      !isRemovedMessage(
        scope.stateDir,
        target.kind || "agent",
        target.chat,
        target.message,
      )
    )
      targets.set(
        `${target.chat}:${target.message.clientMessageId || target.message.id}`,
        target,
      );
  };
  for (const entry of outgoing) {
    if (entry.status !== "sending") continue;
    include({
      chat: entry.body.room,
      kind: data.threads.some(
        (agent) => agent.id === entry.body.room && agent.source === "managed",
      )
        ? "agent"
        : "room",
      message: {
        id: `${entry.body.room}:${entry.id}`,
        clientMessageId: entry.id,
        role: "user",
        text: entry.body.text,
        assets: entry.attachments,
        at: entry.created / 1000,
        deliveryStatus: "sending",
      },
    });
  }
  const agents = data.threads.filter((agent) => agent.source === "managed");
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(4, agents.length) }, async () => {
      while (next < agents.length) {
        const agent = agents[next++];
        const queue = await api<Queue>(
          `/api/queue?agent=${encodeURIComponent(agent.id)}`,
          undefined,
          { workspaceId: scope.workspaceId },
        );
        for (const row of queue.items) {
          if ((row.requestedDelivery || row.delivery) === "after_turn")
            continue;
          include({
            chat: agent.id,
            queue,
            message: {
              id: row.id,
              clientMessageId: row.id,
              role: "user",
              text: row.text,
              assets: row.assets,
              at: row.created,
              deliveryStatus: "sending",
            },
          });
        }
        if (["running", "starting", "approval"].includes(agent.status)) {
          const page = await api<{ items: Message[] }>(
            `/api/transcript/page?id=${encodeURIComponent(agent.id)}`,
            undefined,
            { workspaceId: scope.workspaceId },
          );
          for (const message of transcriptMessages(page.items, agent.id))
            if (isSendingMessage(message)) include({ chat: agent.id, message });
        }
      }
    }),
  );
  let removed = 0;
  for (const target of targets.values()) {
    // Re-read each queue revision after a previous cancellation in that chat.
    await removeSendingMessage(scope, { ...target, queue: undefined });
    removed++;
  }
  return removed;
}
