import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
import { inputText } from "./sdk-fixtures.ts";

import fs from "node:fs";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const usage = { input_tokens: 10, output_tokens: 2 };
export function query({ prompt, options }: FixtureQuery) {
  let reconnected = false;
  const abort = new AbortController(),
    log = (event: unknown) =>
      fs.appendFileSync(options.cwd + "/.log", JSON.stringify(event) + "\n");
  log({
    query: {
      resume: options.resume ?? null,
      sessionId: options.sessionId ?? null,
    },
  });
  return {
    mcpServerStatus: async () => [
      {
        name: "one",
        status: "connected",
        scope: "project",
        tools: [{ name: "read" }],
        config: { env: { TOKEN: "private" } },
      },
      {
        name: "two",
        status: reconnected ? "connected" : "failed",
        error: reconnected ? undefined : "offline",
      },
    ],
    reconnectMcpServer: async (name: string) => {
      if (name !== "two") throw new Error("unknown server");
      reconnected = true;
      log({ reconnect: name });
    },
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
      if (!abort.signal.aborted) log({ close: true });
      abort.abort();
    },
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      while (!abort.signal.aborted) {
        const input = await prompt.next();
        if (input.done) return;
        log({ input: inputText(input.value) });
        yield { type: "user", isReplay: true, uuid: input.value.uuid };
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
        yield { type: "result", subtype: "success", usage, result: "done" };
      }
    },
  };
}
