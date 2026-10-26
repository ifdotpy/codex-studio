export { resolveSettings } from "./sdk-fixtures.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const events: unknown[] = JSON.parse(process.env.STUDIO_FIXTURE_EVENTS ?? "[]");
export function query({ prompt, options }: FixtureQuery) {
  const abort = new AbortController();
  return {
    supportedModels: async () => [
      {
        value: "sonnet",
        displayName: "Sonnet",
        resolvedModel: "claude-sonnet-4-6",
      },
    ],
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "Claude Max",
      apiProvider: "firstParty",
    }),
    initializationResult: async () => ({ commands: [] }),
    mcpServerStatus: async () => [],
    setModel: async () => {},
    setPermissionMode: async () => {},
    applyFlagSettings: async () => {},
    setMcpServers: async () => ({ added: [], removed: [], errors: {} }),
    close() {
      abort.abort();
    },
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
      for (const event of events) yield event;
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
