import type { SessionFixtureQuery } from "./fixtures/session-sdk-types.ts";
import { waitForFile } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  getSessionMessages,
  forkSession,
  resolveSettings,
} from "./sdk-fixtures.ts";

// Count only this fixture's callback envelope, including any second decode.
let decodes = 0;
const parse = JSON.parse;
JSON.parse = (text, reviver) => {
  if (text.includes('"fixtureCallback":true')) decodes++;
  return parse(text, reviver);
};

export function query({ prompt, options }: SessionFixtureQuery) {
  const abort = new AbortController();
  return {
    supportedModels: async () => [],
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "Claude Max",
      apiProvider: "firstParty",
    }),
    initializationResult: async () => ({ commands: [] }),
    close: () => abort.abort(),
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator]() {
      const input = await prompt.next();
      if (input.done) return;
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      try {
        const value = await options.onElicitation?.(
          {
            serverName: "fixture",
            message: "Fixture callback",
            requestedSchema: { type: "object" },
          },
          { signal: abort.signal, requestId: "fixture-callback" },
        );
        process.stdout.write(
          JSON.stringify({
            method: "fixture/callback",
            params: { value, decodes },
          }) + "\n",
        );
      } catch (error) {
        process.stdout.write(
          JSON.stringify({
            method: "fixture/callback",
            params: {
              error: error instanceof Error ? error.message : String(error),
              decodes,
            },
          }) + "\n",
        );
      }
      await waitForFile(options.cwd + "/.close-callback", {
        signal: abort.signal,
      });
    },
  };
}
