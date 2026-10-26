import { waitForFile as waitForFileSignal } from "./testing/file-signals.ts";
const waitForFile: typeof waitForFileSignal = (file, options) =>
  waitForFileSignal(file, { ...options, childIpc: true });
import type { FixtureQuery } from "./sdk-fixtures.ts";

import fs from "node:fs";
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
  let moved = () => {};
  const movedPromise = new Promise<void>((resolve) => (moved = resolve));
  let foreground = false;
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
    backgroundTasks: async (toolUseId: string) => {
      fs.appendFileSync(cwd + "/.background-calls", String(toolUseId) + "\n");
      if (!foreground || toolUseId !== "toolu-1") return false;
      foreground = false;
      moved();
      return true;
    },
    close() {
      abort.abort();
    },
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator]() {
      const input = await prompt.next();
      if (input.done) return;
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      yield {
        type: "system",
        subtype: "init",
        session_id: options.resume || options.sessionId,
      };
      yield {
        type: "stream_event",
        event: { type: "message_start", message: { id: "message-1" } },
      };
      yield {
        type: "assistant",
        uuid: "a-1",
        message: {
          id: "message-1",
          model: "claude-opus-5-5",
          usage,
          content: [
            {
              type: "tool_use",
              id: "toolu-1",
              name: "Bash",
              input: { command: "sleep 600" },
            },
          ],
        },
      };
      await until(cwd + "/.registered", abort.signal);
      if (abort.signal.aborted) return;
      foreground = true;
      yield {
        type: "system",
        subtype: "task_started",
        task_id: "bash-1",
        tool_use_id: "toolu-1",
        task_type: "local_bash",
        is_backgrounded: false,
        description: "sleep 600",
        uuid: "s-1",
      };
      await movedPromise;
      yield {
        type: "system",
        subtype: "task_updated",
        task_id: "bash-1",
        patch: { is_backgrounded: true },
        uuid: "s-2",
      };
      yield {
        type: "user",
        uuid: "u-1",
        message: {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "toolu-1",
              content:
                "Command running in background with ID: bash-1. Output is being written to: /tmp/bash-1.output",
            },
          ],
        },
        tool_use_result: {
          stdout: "",
          stderr: "",
          interrupted: false,
          backgroundTaskId: "bash-1",
          backgroundedByUser: true,
        },
      };
      yield { type: "result", subtype: "success", usage, result: "continued" };
      await until(cwd + "/.finish", abort.signal);
      if (abort.signal.aborted) return;
      yield {
        type: "system",
        subtype: "task_notification",
        task_id: "bash-1",
        tool_use_id: "toolu-1",
        status: "completed",
        output_file: "/tmp/bash-1.output",
        summary: "Background command completed (exit code 0)",
        uuid: "s-3",
      };
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
