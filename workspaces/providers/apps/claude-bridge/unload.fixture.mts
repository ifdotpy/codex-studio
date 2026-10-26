import type { FixtureQuery } from "./sdk-fixtures.ts";
import { inputText } from "./sdk-fixtures.ts";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import type { UserDialogResult } from "@anthropic-ai/claude-agent-sdk";
export interface UnloadLog {
  query?: { resume: string | null; sessionId: string | null };
  input?: string | undefined;
  close?: true;
  dialog?: UserDialogResult | null | undefined;
}

import fs from "node:fs";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const usage = { input_tokens: 10, output_tokens: 2 };
export function fixtureQuery(
  { prompt, options }: FixtureQuery,
  dialog = false,
) {
  const abort = new AbortController(),
    log = (event: UnloadLog) =>
      fs.appendFileSync(options.cwd + "/.log", JSON.stringify(event) + "\n");
  let asked = false;
  log({
    query: {
      resume: options.resume ?? null,
      sessionId: options.sessionId ?? null,
    },
  });
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
        if (dialog && options.resume && !asked) {
          asked = true;
          log({
            dialog: await options.onUserDialog?.(
              { dialogKind: "resume_return", toolUseID: "dialog", payload: {} },
              { requestId: "dialog", signal: abort.signal },
            ),
          });
        }
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

export const query = (args: FixtureQuery) => fixtureQuery(args);
