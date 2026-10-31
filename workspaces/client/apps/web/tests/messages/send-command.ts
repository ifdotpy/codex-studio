// Fixtures for the `send` command of `POST /api/commands`. The old message route is gone; a spec
// that mocked it handles the command here, with the old body and the old receipt.
import type { Request, Route } from "@playwright/test";
import type {
  CommandRequest,
  CommandResponse,
  MessageDelivery,
  SendMessageBody,
} from "../../src/commands.ts";

export type { MessageDelivery, SendMessageBody };

type Send = Extract<CommandRequest, { kind: "send" }>;

/** The command of a request, when it is a command of `kind`. */
export function commandOf<K extends CommandRequest["kind"]>(
  request: Pick<Request, "method" | "postDataJSON">,
  kind: K,
): Extract<CommandRequest, { kind: K }> | undefined {
  if (request.method() !== "POST") return undefined;
  const command = request.postDataJSON() as CommandRequest | null;
  return command?.kind === kind
    ? (command as Extract<CommandRequest, { kind: K }>)
    : undefined;
}

/** The message that a send command creates, in the shape of the old message route body. */
export function sendBody(command: Send): SendMessageBody {
  return { id: command.id, room: command.target, ...command.body };
}

/** The send command of a request, as the old message body. */
export function sentMessage(
  request: Pick<Request, "method" | "postDataJSON">,
): SendMessageBody | undefined {
  const command = commandOf(request, "send");
  return command ? sendBody(command) : undefined;
}

/** The response of the command path that carries `receipt` as the delivery receipt row. */
export function sendReply(receipt: MessageDelivery): CommandResponse {
  return {
    status: "applied",
    ref_kind: "message",
    ref_id: receipt.id,
    rev: 1,
    seq: 0,
    rows: [
      {
        collection: "messageReceipt",
        id: receipt.id,
        seq: 0,
        payload: receipt as unknown as Record<string, unknown>,
      },
    ],
  };
}

/**
 * Wrap a handler written for the old message route. Other commands and other methods go on
 * to the next handler. The handler gets the old body and a `reply` that fulfills the receipt.
 */
export function sendRoute(
  handler: (
    route: Route,
    body: SendMessageBody,
    reply: (receipt: MessageDelivery) => Promise<void>,
  ) => unknown,
): (route: Route) => Promise<void> {
  return async (route) => {
    const body = sentMessage(route.request());
    if (!body) return route.fallback();
    await handler(route, body, (receipt) =>
      route.fulfill({ json: sendReply(receipt) }),
    );
  };
}
