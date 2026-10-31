// Client of the one command path: `POST /api/commands`.
// Tracking: https://github.com/ifdotpy/layr-studio/issues/630
//
// The client makes the command id and numbers its commands. The server keeps the highest
// applied number of each client (the watermark), so the number must never go down. Both
// values are saved before the request, so a retry after a lost response reuses them.
import type { components } from "./generated/api.ts";
import type { PendingCommands } from "./pendingCommands.ts";
import { randomUUID } from "./uuid.ts";

export type CommandRequest = components["schemas"]["CommandRequest"];
export type CommandResponse = components["schemas"]["CommandResponse"];
export type CommandKind = CommandRequest["kind"];
type Variant<K extends CommandKind> = Extract<CommandRequest, { kind: K }>;

/** The message that the outbox stores and the `send` command creates. */
export type SendMessageBody = components["schemas"]["ManagedMessageBody"];
/** The delivery receipt of a message, as `GET /api/messages/receipts` returns it. */
export type MessageDelivery =
  components["schemas"]["studio_api__work__models__MessageReceipt"];

/** The delivery receipt that a `send` response carries as a row. */
export function messageReceipt(
  response: CommandResponse,
): MessageDelivery | undefined {
  return response.rows.find((row) => row.collection === "messageReceipt")
    ?.payload as MessageDelivery | undefined;
}

/** Key of the saved client identity. One browser storage holds one client per server. */
export const COMMAND_CLIENT_KEY = "studio-command-client";

type ClientState = { client: string; number: number };

function readState(storage: Storage): ClientState | undefined {
  try {
    const value: unknown = JSON.parse(
      storage.getItem(COMMAND_CLIENT_KEY) ?? "",
    );
    if (
      typeof value === "object" &&
      value !== null &&
      "client" in value &&
      "number" in value &&
      typeof value.client === "string" &&
      value.client !== "" &&
      typeof value.number === "number" &&
      Number.isSafeInteger(value.number) &&
      value.number >= 0
    )
      return { client: value.client, number: value.number };
  } catch {
    // A missing or damaged record starts a new client below.
  }
  return undefined;
}

/** Take the next command number of this client. The number never repeats or goes down. */
export function nextCommandNumber(
  storage: Storage,
  memory: { state?: ClientState },
): ClientState {
  const state = readState(storage) ??
    memory.state ?? {
      client: randomUUID(),
      number: 0,
    };
  const next = { client: state.client, number: state.number + 1 };
  memory.state = next;
  try {
    storage.setItem(COMMAND_CLIENT_KEY, JSON.stringify(next));
  } catch {
    // Storage is blocked. The number still rises for this page.
  }
  return next;
}

let standaloneNumber = Date.now();
/**
 * A command for a caller without a saved client (a script or a test). It uses one fixed client
 * and numbers that rise within the process and start at the clock.
 */
export function standaloneCommand<K extends CommandKind>(
  kind: K,
  target: string,
  body: Variant<K>["body"],
  id: string = randomUUID(),
): Variant<K> {
  return {
    id,
    client: "standalone",
    number: ++standaloneNumber,
    kind,
    target,
    body,
  } as Variant<K>;
}

export type CommandPost = (
  body: CommandRequest,
  options?: {
    workspaceId?: string | undefined;
    sessionToken?: string | undefined;
  },
) => Promise<CommandResponse>;

type CommandOptions = {
  workspaceId?: string | undefined;
  sessionToken?: string | undefined;
};

export function createCommandClient(
  storage: Storage,
  post: CommandPost,
  pending?: PendingCommands,
) {
  const memory: { state?: ClientState } = {};
  /** Build a command with a new id and the next number. Keep it to retry the same command. */
  function make<K extends CommandKind>(
    kind: K,
    target: string,
    body: Variant<K>["body"],
    id: string = randomUUID(),
  ): Variant<K> {
    const { client, number } = nextCommandNumber(storage, memory);
    return {
      id,
      client,
      number,
      kind,
      target,
      body,
    } as Variant<K>;
  }
  /** Show the expected result at once, then send. A retry sends the same object. */
  async function run(command: CommandRequest, options?: CommandOptions) {
    pending?.begin({
      id: command.id,
      kind: command.kind,
      target: command.target,
      ...(command.kind === "withdraw" ? { message: command.body.message } : {}),
    });
    try {
      const response = await post(command, options);
      pending?.settle(command.id, response.seq);
      return response;
    } catch (error) {
      pending?.fail(command.id);
      throw error;
    }
  }
  return {
    make,
    run,
    stop: (chat: string, descendants: boolean, options?: CommandOptions) =>
      run(make("stop", chat, { descendants }), options),
    withdraw: (chat: string, message: string, options?: CommandOptions) =>
      run(make("withdraw", chat, { message }), options),
    /** The message id is the command id, so a retry of the same message is the same command. */
    send: (message: SendMessageBody, options?: CommandOptions) =>
      run(
        make(
          "send",
          message.room,
          {
            text: message.text,
            ...(message.assets ? { assets: message.assets } : {}),
            ...(message.delivery ? { delivery: message.delivery } : {}),
          },
          message.id,
        ),
        options,
      ),
  };
}

export type CommandClient = ReturnType<typeof createCommandClient>;
