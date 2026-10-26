import { randomUUID } from "node:crypto";
import type {
  SessionFixtureQuery,
  SessionFixtureMessage,
} from "./session-sdk-types.ts";
export const resolveSettings = async () => ({ effective: {} });
export const tool = (
  name: string,
  description: string,
  schema: unknown,
  call: unknown,
) => ({ name, description, schema, call });
export const createSdkMcpServer = (value: Record<string, unknown>) => ({
  ...value,
});
export const forkSession = async () => ({
  sessionId: "22222222-2222-4222-8222-222222222222",
});
export const getSessionMessages = async () => [];
export function query({ prompt, options }: SessionFixtureQuery) {
  const abort = new AbortController();
  return {
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
    mcpServerStatus: async () => [],
    reconnectMcpServer: async () => {},
    setModel: async () => {},
    setPermissionMode: async () => {},
    applyFlagSettings: async () => {},
    stopTask: async () => {},
    setMcpServers: async () => ({ added: [], removed: [], errors: {} }),
    close() {
      abort.abort();
    },
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator](): AsyncGenerator<SessionFixtureMessage> {
      for await (const input of prompt) {
        if (abort.signal.aborted) return;
        yield { type: "user", isReplay: true, uuid: input.uuid };
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
        if (
          Array.isArray(input.message.content) &&
          input.message.content[0]?.type === "text" &&
          input.message.content[0].text === "/compact"
        )
          yield {
            type: "system",
            subtype: "compact_boundary",
            uuid: randomUUID(),
            compact_metadata: {
              trigger: "manual",
              pre_tokens: 210000,
              post_tokens: 1000,
            },
          };
        yield {
          type: "assistant",
          uuid: randomUUID(),
          message: {
            id: "response",
            role: "assistant",
            model: "claude-opus-5-5",
            content: [{ type: "text", text: "done" }],
            usage: {
              input_tokens: 10,
              cache_read_input_tokens: 200000,
              cache_creation_input_tokens: 10000,
              output_tokens: 20,
              cache_creation: {
                ephemeral_1h_input_tokens: 10000,
                ephemeral_5m_input_tokens: 0,
              },
            },
          },
        };
        yield {
          type: "result",
          subtype: "success",
          usage: { input_tokens: 9999999, output_tokens: 9999999 },
          result: "done",
        };
      }
    },
  };
}
