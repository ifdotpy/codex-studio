export { resolveSettings } from "./sdk-fixtures.ts";
import { inputText } from "./sdk-fixtures.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const usage = { input_tokens: 10, output_tokens: 2 };
const none = { input_tokens: 0, output_tokens: 0 };
const info = {
  status: "rejected",
  resetsAt: 1900000000,
  rateLimitType: "five_hour",
  overageStatus: "rejected",
  overageDisabledReason: "org_level_disabled",
  isUsingOverage: false,
};
const refused = (id: string, details: boolean) => ({
  type: "assistant",
  uuid: "a-" + id,
  parent_tool_use_id: null,
  message: {
    id,
    type: "message",
    role: "assistant",
    model: "<synthetic>",
    stop_reason: "stop_sequence",
    usage: none,
    content: [
      {
        type: "text",
        text: "You've hit your session limit · resets 3:40am (UTC)",
      },
    ],
  },
  error: "rate_limit",
  is_api_error_message: true,
  api_error_status: 429,
  ...(details
    ? {
        api_error: "usage_limit_reached",
        api_error_params: { rate_limit_info: info },
      }
    : {}),
});
const failed = {
  type: "result",
  subtype: "success",
  is_error: true,
  usage: none,
  result: "You've hit your session limit · resets 3:40am (UTC)",
};
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
    async *[Symbol.asyncIterator]() {
      const input = await prompt.next();
      if (input.done) return;
      const mode = inputText(input.value);
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      if (mode === "plain") {
        yield refused("refused-1", false);
        yield failed;
      } else {
        yield {
          type: "stream_event",
          event: { type: "message_start", message: { id: "message-1" } },
        };
        yield {
          type: "assistant",
          uuid: "a-1",
          message: {
            id: "message-1",
            model: "claude-opus-5-5",
            usage,
            content: [{ type: "text", text: "started" }],
          },
        };
        yield { type: "result", subtype: "success", usage, result: "started" };
        yield { type: "rate_limit_event", rate_limit_info: info, uuid: "r-1" };
        yield refused("refused-2", true);
        yield failed;
      }
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
