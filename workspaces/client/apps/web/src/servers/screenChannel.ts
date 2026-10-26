import type { ServerCommand } from "./navigation";
import type { GetResult } from "../api";

export type ScreenMessage = { kind: string; [key: string]: unknown };
export type ScreenSearchResults = Pick<GetResult<"/api/search">, "results"> & {
  error?: string | undefined;
};
export type ScreenSearchResultsMessage = ScreenMessage &
  Partial<ScreenSearchResults> & {
    kind: "studio-server-search-results";
    requestId: string;
  };

/** Search replies come from the same renderer's generated API client. */
export function screenSearchResults(
  message: ScreenMessage,
): ScreenSearchResults {
  const value = message as ScreenSearchResultsMessage;
  return { results: value.results || [], error: value.error };
}
type CommandHandler = (command: ServerCommand) => void | Promise<void>;

/** One connection lifetime owns its commands, even after another screen opens. */
export function createScreenChannel(
  owner: string,
  publish: (message: ScreenMessage & { serverId: string }) => void,
) {
  let closed = false;
  let handler: CommandHandler | undefined;
  let terminal: ((operation: "toggle" | "new") => void) | undefined;
  return {
    owner,
    get ready() {
      return !closed && !!handler;
    },
    publish(message: ScreenMessage) {
      if (!closed) publish({ ...message, serverId: owner });
    },
    bind(next: CommandHandler) {
      if (closed) throw new Error("The server screen connection closed.");
      if (handler) throw new Error("The server screen is already mounted.");
      handler = next;
      return () => {
        if (handler === next) handler = undefined;
      };
    },
    bindTerminal(next: (operation: "toggle" | "new") => void) {
      if (closed) throw new Error("The server screen connection closed.");
      terminal = next;
      return () => {
        if (terminal === next) terminal = undefined;
      };
    },
    async execute(command: ServerCommand) {
      if (closed || !handler)
        throw new Error(
          "The server screen is unavailable. The action was not sent.",
        );
      if (command.action === "terminal") {
        if (!terminal)
          throw new Error(
            "The server terminal is unavailable. The action was not sent.",
          );
        terminal(command.operation);
        return;
      }
      // No retries: the handler may have sent a mutation before its reply failed.
      await handler(command);
    },
    close() {
      closed = true;
      handler = undefined;
      terminal = undefined;
    },
  };
}
export type ScreenChannel = ReturnType<typeof createScreenChannel>;
