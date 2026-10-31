// Client of the one command path: `POST /api/commands`.
// Tracking: https://github.com/ifdotpy/layr-studio/issues/630
//
// The client makes the command id and numbers its commands. The server keeps the highest
// applied number of each client (the watermark), so the number must never go down. Both
// values are saved before the request, so a retry after a lost response reuses them.
import type { components } from "./generated/api";
import { randomUUID } from "./uuid";

export type CommandRequest = components["schemas"]["CommandRequest"];
export type CommandResponse = components["schemas"]["CommandResponse"];
export type CommandKind = CommandRequest["kind"];
type Variant<K extends CommandKind> = Extract<CommandRequest, { kind: K }>;

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

export type CommandPost = (
  body: CommandRequest,
  options?: {
    workspaceId?: string | undefined;
    sessionToken?: string | undefined;
  },
) => Promise<CommandResponse>;

export function createCommandClient(storage: Storage, post: CommandPost) {
  const memory: { state?: ClientState } = {};
  /** Build a command with a new id and the next number. Keep it to retry the same command. */
  function make<K extends CommandKind>(
    kind: K,
    target: string,
    body: Variant<K>["body"],
  ): Variant<K> {
    const { client, number } = nextCommandNumber(storage, memory);
    return {
      id: randomUUID(),
      client,
      number,
      kind,
      target,
      body,
    } as Variant<K>;
  }
  return {
    make,
    /** Send a command built by `make`. A retry sends the same object. */
    run: (
      command: CommandRequest,
      options?: {
        workspaceId?: string | undefined;
        sessionToken?: string | undefined;
      },
    ) => post(command, options),
    stop: (
      chat: string,
      descendants: boolean,
      options?: { workspaceId?: string | undefined },
    ) => post(make("stop", chat, { descendants }), options),
    withdraw: (
      chat: string,
      message: string,
      options?: { workspaceId?: string | undefined },
    ) => post(make("withdraw", chat, { message }), options),
  };
}

export type CommandClient = ReturnType<typeof createCommandClient>;
