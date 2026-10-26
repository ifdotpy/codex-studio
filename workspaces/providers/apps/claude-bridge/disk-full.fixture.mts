import { randomUUID } from "node:crypto";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import { waitForFile as waitForFileSignal } from "./testing/file-signals.ts";
const waitForFile: typeof waitForFileSignal = (file, options) =>
  waitForFileSignal(file, { ...options, childIpc: true });
import type { FixtureQuery } from "./sdk-fixtures.ts";
import { inputText } from "./sdk-fixtures.ts";

import fs from "node:fs";
import fsp from "node:fs/promises";
const marker = process.cwd() + "/.disk-full";
const rename = fsp.rename;
fsp.rename = async (from, to) => {
  if (fs.existsSync(marker))
    throw Object.assign(
      new Error(
        "ENOSPC: no space left on device, rename '" +
          from +
          "' -> '" +
          to +
          "'",
      ),
      { code: "ENOSPC" },
    );
  return rename(from, to);
};
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const until = (file: string, signal: AbortSignal) =>
  waitForFile(file, { signal }).catch(() => {});
const usage = { input_tokens: 10, output_tokens: 2 };
export function query({ prompt, options }: FixtureQuery) {
  const abort = new AbortController(),
    cwd = options.cwd;
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
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      const input = await prompt.next();
      if (input.done) return;
      fs.appendFileSync(cwd + "/.inputs", inputText(input.value) + "\n");
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      fs.writeFileSync(marker, "");
      yield {
        type: "stream_event",
        event: { type: "message_start", message: { id: "message-1" } },
      };
      yield {
        type: "assistant",
        uuid: randomUUID(),
        message: {
          id: "message-1",
          model: "claude-opus-5-5",
          usage,
          content: [{ type: "text", text: "still working" }],
        },
      };
      await until(cwd + "/.space", abort.signal);
      if (abort.signal.aborted) return;
      fs.rmSync(marker, { force: true });
      yield { type: "result", subtype: "success", usage, result: "done" };
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
