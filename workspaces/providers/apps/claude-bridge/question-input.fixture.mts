import type { FixtureQuery } from "./sdk-fixtures.ts";
import { sdkResult } from "./usage-fixtures.ts";
export {
  resolveSettings,
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
export function query({ prompt, options }: FixtureQuery) {
  const abort = new AbortController();
  return {
    supportedModels: async () => [],
    mcpServerStatus: async () => [],
    initializationResult: async () => ({ commands: [] }),
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "Claude Max",
      apiProvider: "firstParty",
    }),
    setModel: async () => {},
    setPermissionMode: async () => {},
    applyFlagSettings: async () => {},
    setMcpServers: async () => ({ added: [], removed: [], errors: {} }),
    close: () => abort.abort(),
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator]() {
      const input = await prompt.next();
      if (input.done) return;
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      const questions: unknown = JSON.parse(process.env.STUDIO_QUESTION_INPUT!);
      const result = await options.canUseTool!(
        "AskUserQuestion",
        { questions },
        {
          toolUseID: "question-one",
          requestId: "question-one",
          signal: abort.signal,
        },
      );
      process.stdout.write(
        JSON.stringify({ method: "fixture/permission", params: result }) + "\n",
      );
      yield sdkResult;
    },
  };
}
