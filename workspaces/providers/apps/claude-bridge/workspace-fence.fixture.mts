export { resolveSettings } from "./sdk-fixtures.ts";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import type { FixtureTextQuery as FixtureQuery } from "./sdk-fixtures.ts";

import { writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
export {
  tool,
  createSdkMcpServer,
  getSessionMessages,
} from "./sdk-fixtures.ts";
// A test selects one extra observation; the default record stays exact.
const record = process.env.STUDIO_FENCE_FIXTURE_RECORD;
export const forkSession = async () => ({ sessionId: "fork" });
export function query({ prompt, options }: FixtureQuery) {
  writeFileSync(
    path.join(options.cwd, "sdk-env.json"),
    JSON.stringify({
      tree: options.env!.CLAUDE_CODE_HOST_WORKTREE,
      fence: options.env!.CLAUDE_CODE_HOST_WORKTREE_FENCE,
      permissionMode: options.permissionMode,
      account: options.env?.STUDIO_CLAUDE_ACCOUNT,
      ...(record === "native"
        ? {
            native: {
              version: spawnSync("layr", ["--version"], {
                env: options.env,
                encoding: "utf8",
              }).stdout,
              root: options.env?.LAYR_ROOT,
              direct: options.env?.LAYR_DIRECT,
              socket: options.env?.LAYR_SOCKET,
              tmp: options.env?.TMPDIR,
            },
          }
        : {}),
      ...(record === "sandbox" ? { sandbox: options.sandbox } : {}),
      ...(record === "environment" ? { environment: options.env } : {}),
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
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      const input = await prompt.next();
      if (input.done) return;
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "result",
        subtype: "success",
        usage: { input_tokens: 1, output_tokens: 1 },
        result: "done",
      };
      await new Promise<unknown>((resolve) =>
        abort.signal.addEventListener("abort", resolve, { once: true }),
      );
    },
  };
}
