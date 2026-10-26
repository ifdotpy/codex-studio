export { resolveSettings } from "./sdk-fixtures.ts";
import { randomUUID } from "node:crypto";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
import { inputText } from "./sdk-fixtures.ts";

export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const usage = { input_tokens: 10, output_tokens: 2 };
export function query({ prompt, options }: FixtureQuery) {
  const abort = new AbortController();
  return {
    mcpServerStatus: async () => [],
    reconnectMcpServer: async () => {},
    supportedModels: async () => [
      {
        value: "default",
        displayName: "Default",
        resolvedModel: "claude-opus-5-5",
      },
    ],
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "Claude Max",
      apiProvider: "firstParty",
    }),
    initializationResult: async () => ({ commands: [] }),
    setModel: async () => {},
    setPermissionMode: async () => {},
    applyFlagSettings: async () => {},
    stopTask: async () => {},
    setMcpServers: async () => ({ added: [], removed: [], errors: {} }),
    close() {
      abort.abort();
    },
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      const input = await prompt.next();
      if (input.done) return;
      const count = Number(inputText(input.value));
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      yield {
        type: "stream_event",
        event: { type: "message_start", message: { id: "message-1" } },
      };
      const blocks = ["", ""];
      for (const index of [0, 1])
        for (let i = 0; i < count; i++) {
          const text = "b" + index + "w" + i + " ";
          blocks[index] += text;
          yield {
            type: "stream_event",
            event: {
              type: "content_block_delta",
              index,
              delta: { type: "text_delta", text },
            },
          };
        }
      yield {
        type: "assistant",
        uuid: randomUUID(),
        message: {
          id: "message-1",
          model: "claude-opus-5-5",
          usage,
          content: blocks.map((text) => ({ type: "text", text })),
        },
      };
      yield { type: "result", subtype: "success", usage, result: "done" };
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
