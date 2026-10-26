export { resolveSettings } from "./sdk-fixtures.ts";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
import { isRecord } from "./move.ts";

function textOf(content: unknown): string {
  const block: unknown = Array.isArray(content) ? content[0] : undefined;
  return isRecord(block) && typeof block.text === "string" ? block.text : "";
}

export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";

// The first user message names the scripted failure. The fixture yields it as
// the assistant error, the terminal result, or a thrown SDK error.
export function query({ prompt, options }: FixtureQuery) {
  const abort = new AbortController();
  return {
    mcpServerStatus: async () => [],
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
      const config: Record<string, unknown> = JSON.parse(
        String(textOf(input.value.message.content)),
      );
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      if (config.thrown)
        throw Object.assign(new Error("Provider failed"), config.thrown);
      if (config.assistant)
        yield {
          type: "assistant",
          uuid: "11111111-1111-4111-8111-111111111111",
          parent_tool_use_id: null,
          message: {
            id: "failure",
            model: "<synthetic>",
            usage: { input_tokens: 0, output_tokens: 0 },
            content: [{ type: "text", text: "Provider failed" }],
          },
          ...(isRecord(config.assistant) ? config.assistant : {}),
        };
      yield {
        type: "result",
        subtype: "success",
        is_error: true,
        usage: { input_tokens: 0, output_tokens: 0 },
        result: "Provider failed",
        ...(isRecord(config.result) ? config.result : {}),
      };
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
