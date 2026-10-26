const scenario = process.env.STUDIO_FIXTURE_FINAL_SAVE;
import type { FixtureQuery } from "./sdk-fixtures.ts";
import { inputText } from "./sdk-fixtures.ts";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";

import fs from "node:fs/promises";
let cwd: string;
const append = fs.appendFile;
fs.appendFile = async (file, data, ...args) => {
  if (
    scenario !== "success" &&
    String(file).includes("/sessions/") &&
    String(data).includes('"value":"completed"')
  ) {
    await append(cwd + "/save-attempts", "x");
    throw Object.assign(new Error("final save denied"), { code: "EACCES" });
  }
  return append(file, data, ...args);
};
export { tool, createSdkMcpServer } from "./sdk-fixtures.ts";
export const forkSession = async () => {
  throw new Error("No fork expected");
};
export const getSessionMessages = async () => [];
export function query({ prompt, options }: FixtureQuery) {
  cwd = options.cwd;
  let closed = false;
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
    close() {
      closed = true;
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      const first = await prompt.next();
      if (first.done) return;
      await fs.appendFile(options.cwd + "/inputs", "once\n");
      yield { type: "user", isReplay: true, uuid: first.value.uuid };
      yield {
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      if (scenario === "timer") {
        const steer = await prompt.next();
        if (steer.done) return;
        await fs.appendFile(cwd + "/inputs", inputText(steer.value) + "\n");
      }
      yield {
        type: "result",
        subtype: "success",
        result: "done",
        usage: { input_tokens: 1, output_tokens: 1 },
        modelUsage: {
          "claude-opus-5-5": {
            inputTokens: 1,
            outputTokens: 1,
            cacheReadInputTokens: 0,
            cacheCreationInputTokens: 0,
            webSearchRequests: 0,
            costUSD: 0,
            contextWindow: 200000,
            maxOutputTokens: 64000,
          },
        },
      };
      if (!closed) await prompt.next();
    },
  };
}
