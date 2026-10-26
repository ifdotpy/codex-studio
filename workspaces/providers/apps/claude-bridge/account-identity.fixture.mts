import type { FixtureQuery } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
export function query({ prompt }: FixtureQuery) {
  return {
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "max",
      apiProvider: "firstParty",
    }),
    supportedModels: async () => [
      {
        value: "default",
        displayName: "Default",
        resolvedModel: "claude-opus-5-5",
      },
    ],
    initializationResult: async () => ({ commands: [] }),
    close() {},
    async *[Symbol.asyncIterator]() {
      const input = await prompt.next();
      if (!input.done) throw new Error("INPUT MUST NOT BE SUBMITTED");
    },
  };
}
