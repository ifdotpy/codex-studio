export { resolveSettings } from "./sdk-fixtures.ts";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import { waitForFile } from "./sdk-fixtures.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";

import fs from "node:fs";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const usage = { input_tokens: 1, output_tokens: 1 };
// A slow MCP call. Native auto-background moves it to a task only when the
// bridge passes the enabled environment; the opt-out waits in the foreground.
export function query({ prompt, options }: FixtureQuery) {
  const abort = new AbortController(),
    cwd = options.cwd;
  const env = options.env ?? {};
  return {
    mcpServerStatus: async () => [],
    supportedModels: async () => [],
    initializationResult: async () => ({ commands: [] }),
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "Claude Max",
      apiProvider: "firstParty",
    }),
    close() {
      abort.abort();
    },
    interrupt: async () => {
      fs.writeFileSync(cwd + "/.interrupted", "yes");
      abort.abort();
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      const input = await prompt.next();
      if (input.done) return;
      fs.writeFileSync(cwd + "/options.json", JSON.stringify(options));
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "assistant",
        message: {
          id: "call",
          usage,
          content: [
            {
              type: "tool_use",
              id: "mcp-call",
              name: "mcp__remote__slow",
              input: {},
            },
          ],
        },
      };
      if (env.CLAUDE_AUTO_BACKGROUND_TASKS !== "true") {
        await waitForFile(cwd + "/.finish", { signal: abort.signal }).catch(
          () => {},
        );
        return;
      }
      await new Promise((resolve) =>
        setTimeout(resolve, Number(env.CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS)),
      );
      yield {
        type: "system",
        subtype: "task_started",
        task_id: "mcp-task",
        task_type: "mcp_task",
        tool_use_id: "mcp-call",
        is_backgrounded: true,
        description: "Slow remote tool",
      };
      yield {
        type: "system",
        subtype: "background_tasks_changed",
        tasks: [
          {
            task_id: "mcp-task",
            task_type: "mcp_task",
            description: "Slow remote tool",
          },
        ],
      };
      yield {
        type: "user",
        message: {
          content: [
            {
              type: "tool_result",
              tool_use_id: "mcp-call",
              content: "Task continues in the background",
            },
          ],
        },
      };
      yield {
        type: "result",
        subtype: "success",
        is_error: false,
        usage,
        result: "Waiting in the background",
      };
      await waitForFile(cwd + "/.finish", { signal: abort.signal }).catch(
        () => {},
      );
      if (abort.signal.aborted) return;
      yield {
        type: "system",
        subtype: "task_notification",
        task_id: "mcp-task",
        status: "completed",
        summary: "Remote tool finished",
      };
      yield {
        type: "system",
        subtype: "background_tasks_changed",
        tasks: [],
      };
      yield {
        type: "assistant",
        message: {
          id: "wake",
          usage,
          content: [{ type: "text", text: "Remote tool finished" }],
        },
      };
      yield {
        type: "result",
        subtype: "success",
        is_error: false,
        usage,
        result: "Remote tool finished",
      };
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
