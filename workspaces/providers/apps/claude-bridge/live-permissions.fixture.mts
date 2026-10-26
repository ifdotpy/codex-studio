import type {
  CanUseTool,
  PermissionMode,
} from "@anthropic-ai/claude-agent-sdk";
import type { FixtureQuery } from "./sdk-fixtures.ts";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import { waitForFile } from "./sdk-fixtures.ts";
import fs from "node:fs";
import fsp from "node:fs/promises";
const rename = fsp.rename;
fsp.rename = async (from, to) => {
  const marker = process.cwd() + "/.fail-save";
  if ((to as string).endsWith(".meta.json") && fs.existsSync(marker)) {
    fs.rmSync(marker);
    throw Object.assign(new Error("fixture metadata save failed"), {
      code: "EIO",
    });
  }
  return rename(from, to);
};
export {
  tool,
  createSdkMcpServer,
  getSessionMessages,
  resolveSettings,
} from "./sdk-fixtures.ts";
export const forkSession = async () => ({ sessionId: "fork" });
// The scripted permission callback consumes only the signal and tool-use id.
type PermissionFixtureQuery = Omit<FixtureQuery, "options"> & {
  options: Omit<FixtureQuery["options"], "canUseTool"> & {
    canUseTool: (
      ...args: [
        Parameters<CanUseTool>[0],
        Parameters<CanUseTool>[1],
        Pick<Parameters<CanUseTool>[2], "toolUseID" | "signal">,
      ]
    ) => ReturnType<CanUseTool>;
  };
};
export function query({ prompt, options }: PermissionFixtureQuery) {
  const abort = new AbortController();
  const note = (method: string, params: unknown) =>
    process.stdout.write(JSON.stringify({ method, params }) + "\n");
  return {
    mcpServerStatus: async () => [],
    supportedModels: async () => [],
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "Claude Max",
      apiProvider: "firstParty",
    }),
    initializationResult: async () => ({ commands: [] }),
    setPermissionMode: async (mode: PermissionMode) => {
      note("fixture/mode", { mode });
      if (mode === "plan") throw new Error("fixture live apply failed");
    },
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
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      note("fixture/options", {
        allowed: options.allowDangerouslySkipPermissions,
        mode: options.permissionMode,
      });
      const result = await options.canUseTool(
        "Bash",
        { command: "fixture" },
        {
          toolUseID: "permission-one",
          signal: abort.signal,
        },
      );
      note("fixture/permission", result);
      await waitForFile(options.cwd + "/.ask", { signal: abort.signal });
      const second = await options.canUseTool(
        "Bash",
        { command: "second" },
        {
          toolUseID: "permission-two",
          signal: abort.signal,
        },
      );
      note("fixture/second-permission", second);
      await new Promise((resolve) =>
        abort.signal.addEventListener("abort", resolve, { once: true }),
      );
    },
  };
}
