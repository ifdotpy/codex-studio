import fs from "node:fs";
import path from "node:path";
import type { Query } from "@anthropic-ai/claude-agent-sdk";
import type { FixtureQuery } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
export const resolveSettings = async () => ({ effective: {} });

export function query({ prompt, options }: FixtureQuery) {
  const file = path.join(options.cwd, ".flag-settings.json");
  const state: {
    initial?: typeof options.settings;
    live?: Parameters<Query["applyFlagSettings"]>[0];
  } = {};
  if (options.sessionId || options.resume) {
    state.initial = options.settings;
    fs.writeFileSync(file, JSON.stringify(state));
  }
  return {
    supportedModels: async () => [
      {
        value: "default",
        displayName: "Default",
        description: "Sonnet",
        resolvedModel: "claude-sonnet-4-5",
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
    applyFlagSettings: async (
      flags: Parameters<Query["applyFlagSettings"]>[0],
    ) => {
      state.live = flags;
      fs.writeFileSync(file, JSON.stringify(state));
    },
    close() {},
    async *[Symbol.asyncIterator]() {
      for await (const input of prompt) {
        yield { type: "user", isReplay: true, uuid: input.uuid };
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
        yield {
          type: "result",
          subtype: "success",
          is_error: false,
          usage: { input_tokens: 0, output_tokens: 0 },
          result: "done",
        };
      }
    },
  };
}
