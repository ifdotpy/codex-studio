// Supplied by the bridge harness so waits retain the fixture process IPC lifetime.
declare const waitForFile: typeof import("./testing/file-signals.mjs").waitForFile;
import { inputText } from "./sdk-fixtures.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
import fs from "node:fs";
export function query({ prompt, options }: FixtureQuery) {
  fs.appendFileSync(options.cwd + "/.queries", "q");
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
    setModel: async () => {},
    setPermissionMode: async () => {},
    applyFlagSettings: async () => {},
    setMcpServers: async () => ({ added: [], removed: [], errors: {} }),
    close() {},
    interrupt: async () => {},
    async *[Symbol.asyncIterator]() {
      for await (const input of prompt) {
        process.stdout.write(
          JSON.stringify({ method: "fixture/input", params: input.message }) +
            "\n",
        );
        yield { type: "user", isReplay: true, uuid: input.uuid };
        if (inputText(input) === "hold")
          await waitForFile(options.cwd + "/release");
        yield {
          type: "result",
          subtype: "success",
          usage: { input_tokens: 1, output_tokens: 1 },
          result: "done",
        };
      }
    },
  };
}
