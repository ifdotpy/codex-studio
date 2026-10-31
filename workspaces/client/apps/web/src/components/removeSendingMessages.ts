import { outboxRequests } from "../browserPersistence";
import { AgentSource as GeneratedApiAgentSource } from "../generated/api.ts";
import { DeliveryMoment } from "../generated/deliveryMoment";
import { serverLocalStorage as localStorage } from "../servers/storage";
import { get, command, ApiError } from "../api";
import type { CommandRequest } from "../commands";
import type { Message, Snapshot } from "../types";
import { transcriptMessages } from "../hooks";
import { stopRemovedMessage, type OutgoingMessage } from "../sync/send";
import type { paths } from "../generated/api";
import {
  isRemovedMessage,
  removeMessageFromDevice,
  saveRemovedMessage,
} from "./removedMessages";

export const isSendingMessage = (message: Message) =>
  message.role === "user" &&
  ["sending", "reserved", "dispatching"].includes(message.deliveryStatus || "");

type Queue =
  paths["/api/queue"]["get"]["responses"][200]["content"]["application/json"];
type WithdrawCommand = Extract<CommandRequest, { kind: "withdraw" }>;
type Target = {
  chat: string;
  message: Message;
  queue?: Queue | undefined;
  kind?: string;
};
type Scope = {
  stateDir: string;
  workspaceId?: string | undefined;
  kind?: string;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** A saved withdraw command is kept until it is answered, so a retry repeats the same command. */
function isWithdrawOf(
  value: unknown,
  chat: string,
  message: string,
): value is WithdrawCommand {
  return (
    isRecord(value) &&
    value.kind === "withdraw" &&
    value.target === chat &&
    isRecord(value.body) &&
    value.body.message === message &&
    typeof value.id === "string" &&
    typeof value.client === "string" &&
    typeof value.number === "number"
  );
}

export function createSendingMessageRemoval(
  dependencies: {
    get: typeof get;
    command: Pick<typeof command, "make" | "run">;
    stopRemovedMessage: typeof stopRemovedMessage;
    localStorage: Storage;
  } = {
    get,
    command,
    stopRemovedMessage,
    localStorage,
  },
) {
  const { get, command, stopRemovedMessage, localStorage } = dependencies;
  async function removeSendingMessage(scope: Scope, target: Target) {
    const { chat, message } = target;
    const kind = target.kind || scope.kind || "agent";
    if (!isSendingMessage(message)) return;
    // Preserve a recoverable copy before cancellation can remove a queue receipt.
    saveRemovedMessage(scope.stateDir, kind, chat, message, localStorage);
    const clientId = message.clientMessageId;
    let cancelled = clientId ? await stopRemovedMessage(clientId) : false;
    if (kind === "agent" && clientId && message.deliveryStatus === "sending") {
      const suffix = JSON.stringify([
        scope.stateDir,
        scope.workspaceId,
        chat,
        clientId,
      ]);
      const outbox = outboxRequests(localStorage);
      const saved = outbox.get("removeSending", suffix);
      const parsed: unknown = saved ? JSON.parse(saved) : null;
      if (parsed !== null && !isWithdrawOf(parsed, chat, clientId))
        throw new Error("The saved message cancellation cannot be verified.");
      let request: WithdrawCommand | null = parsed;
      if (!request) {
        const queue =
          target.queue ||
          (await get("/api/queue", {
            query: { agent: chat },
            workspaceId: scope.workspaceId,
          }));
        const row = queue.items.find((item) => item.id === clientId);
        // Internal follow-ups continue. Only user input can be cancelled here.
        if (
          row?.kind === "user" &&
          queue.revision &&
          queue.capabilities?.receipts
        ) {
          request = command.make("withdraw", chat, { message: clientId });
          outbox.set("removeSending", suffix, JSON.stringify(request));
        }
      }
      if (request) {
        try {
          await command.run(request, { workspaceId: scope.workspaceId });
          cancelled = true;
        } catch (error) {
          // Never repeat an unknown mutation with a new identity. A race with
          // native dispatch cannot turn removal into an agent interruption: the
          // message already left the queue (409), or the request was refused (400).
          if (
            !(error instanceof ApiError) ||
            (error.status !== 400 && error.status !== 409)
          )
            throw error;
        }
      }
    }
    removeMessageFromDevice(
      scope.stateDir,
      kind,
      chat,
      {
        ...message,
        deliveryStatus: cancelled ? "cancelled" : "uncertain",
      },
      localStorage,
    );
  }

  async function removeAllSendingMessages(
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
          localStorage,
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
          (agent) =>
            agent.id === entry.body.room &&
            agent.source === GeneratedApiAgentSource.managed,
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
    const agents = data.threads.filter(
      (agent) => agent.source === GeneratedApiAgentSource.managed,
    );
    let next = 0;
    await Promise.all(
      Array.from({ length: Math.min(4, agents.length) }, async () => {
        while (next < agents.length) {
          const agent = agents[next++]!;
          const queue = await get("/api/queue", {
            query: { agent: agent.id },
            workspaceId: scope.workspaceId,
          });
          for (const row of queue.items) {
            if (
              (row.delivery || row.requestedDelivery) ===
              DeliveryMoment.NextTurn
            )
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
          if (
            ["running", "starting", "approval"].includes(agent.status || "")
          ) {
            const page = await get("/api/transcript/page", {
              query: { id: agent.id },
              workspaceId: scope.workspaceId,
            });
            for (const message of transcriptMessages(page.items, agent.id))
              if (isSendingMessage(message))
                include({ chat: agent.id, message });
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

  return { removeSendingMessage, removeAllSendingMessages };
}
export const { removeSendingMessage, removeAllSendingMessages } =
  createSendingMessageRemoval();
