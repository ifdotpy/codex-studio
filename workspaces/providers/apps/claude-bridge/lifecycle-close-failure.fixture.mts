export { resolveSettings } from "./sdk-fixtures.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";

import fs from "node:fs/promises";
export { tool, createSdkMcpServer } from "./sdk-fixtures.ts";
export const forkSession = async () => {
  throw new Error("No fork expected");
};
export const getSessionMessages = async () => [];
export function query({ prompt, options }: FixtureQuery) {
  let consumed = false;
  let held: import("@anthropic-ai/claude-agent-sdk").SDKUserMessage | undefined;
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
    interrupt: async () => ({
      cancelled: [],
      still_queued: held ? [held.uuid] : [],
    }),
    close() {
      if (consumed) throw new Error("SDK close failed");
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      const first = await prompt.next();
      if (first.done) return;
      consumed = true;
      yield { type: "user", isReplay: true, uuid: first.value.uuid };
      yield {
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      held = (await prompt.next()).value;
      await fs.writeFile(options.cwd + "/steer-read", "read");
      await new Promise(() => {});
    },
  };
}
