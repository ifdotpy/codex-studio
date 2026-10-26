import { waitForFile as waitForFileSignal } from "./testing/file-signals.ts";
const waitForFile: typeof waitForFileSignal = (file, options) =>
  waitForFileSignal(file, { ...options, childIpc: true });
import type { FixtureQuery } from "./sdk-fixtures.ts";

import fs from "node:fs";
import fsp from "node:fs/promises";
const wait = waitForFile;
const append = fsp.appendFile;
fsp.appendFile = async (file, data, ...args) => {
  const result = await append(file, data, ...args);
  const root = process.cwd();
  if (
    fs.existsSync(root + "/.hold-save") &&
    String(data).includes("preparing") &&
    !fs.existsSync(root + "/.save-held")
  ) {
    fs.writeFileSync(root + "/.save-held", "");
    await wait(root + "/.release-save");
  }
  return result;
};
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
export function query({ prompt, options }: FixtureQuery) {
  const cwd = options.cwd,
    abort = new AbortController();
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
    accountInfo: async () => {
      if (options.systemPrompt && fs.existsSync(cwd + "/.hold-account")) {
        fs.writeFileSync(cwd + "/.account-entered", "");
        await wait(cwd + "/.release-account");
      }
      return {
        email: "test@example.test",
        subscriptionType: "Claude Max",
        apiProvider: "firstParty",
      };
    },
    initializationResult: async () => ({ commands: [] }),
    setModel: async () => {},
    setPermissionMode: async () => {},
    applyFlagSettings: async () => {},
    stopTask: async () => {},
    setMcpServers: async () => ({ added: [], removed: [], errors: {} }),
    close() {
      abort.abort();
    },
    interrupt: async () => ({ cancelled: [], still_queued: [] }),
    async *[Symbol.asyncIterator]() {
      while (!abort.signal.aborted) {
        const input = await prompt.next();
        if (input.done) return;
        fs.appendFileSync(cwd + "/.admitted", input.value.uuid + "\n");
        yield { type: "user", isReplay: true, uuid: input.value.uuid };
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
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
