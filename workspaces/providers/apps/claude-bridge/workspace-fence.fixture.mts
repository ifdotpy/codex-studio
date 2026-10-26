import type { FixtureQuery } from "./sdk-fixtures.ts";

import { writeFileSync } from "node:fs";
import path from "node:path";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
export function query({ prompt, options }: FixtureQuery) {
  writeFileSync(
    path.join(options.cwd, "sdk-env.json"),
    JSON.stringify({
      tree: options.env?.CLAUDE_CODE_HOST_WORKTREE,
      fence: options.env?.CLAUDE_CODE_HOST_WORKTREE_FENCE,
      permissionMode: options.permissionMode,
      account: options.env?.STUDIO_CLAUDE_ACCOUNT,
    }),
  );
  const abort = new AbortController();
  return {
    mcpServerStatus: async () => [],
    reconnectMcpServer: async () => {},
    supportedModels: async () => [],
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
    async *[Symbol.asyncIterator]() {
      const input = await prompt.next();
      if (input.done) return;
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "result",
        subtype: "success",
        usage: { input_tokens: 1, output_tokens: 1 },
        result: "done",
      };
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
