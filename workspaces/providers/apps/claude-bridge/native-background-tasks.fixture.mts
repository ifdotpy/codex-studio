import { randomUUID } from "node:crypto";
import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import type { SDKTaskNotificationMessage } from "@anthropic-ai/claude-agent-sdk";
// The legacy killed notification deliberately exercises an SDK compatibility boundary.
type NativeTaskFixtureMessage =
  | SessionFixtureMessage
  | Partial<Omit<SDKTaskNotificationMessage, "status"> & { status: "killed" }>;
import { waitForFile } from "./sdk-fixtures.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";

import fs from "node:fs";
import path from "node:path";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
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
    setMcpServers: async () => ({ added: [], removed: [], errors: {} }),
    close() {
      abort.abort();
    },
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator](): AsyncGenerator<
      NativeTaskFixtureMessage,
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
      yield {
        type: "system",
        subtype: "task_started",
        task_id: "native-1",
        task_type: "local_bash",
        is_backgrounded: true,
        description: "Watch the build",
        uuid: randomUUID(),
      };
      yield {
        type: "system",
        subtype: "task_progress",
        task_id: "native-1",
        description: "Watch the build",
        summary: "Build still running",
        uuid: randomUUID(),
      };
      yield {
        type: "system",
        subtype: "task_updated",
        task_id: "native-1",
        patch: { description: "Build almost done", status: "running" },
        uuid: randomUUID(),
      };
      yield {
        type: "result",
        subtype: "success",
        usage,
        result: "Task started",
      };
      await waitForFile(cwd + "/.finish", { signal: abort.signal }).catch(
        () => {},
      );
      if (abort.signal.aborted) return;
      const status = fs.readFileSync(cwd + "/.finish", "utf8");
      if (
        status !== "completed" &&
        status !== "failed" &&
        status !== "killed" &&
        status !== "stopped"
      )
        throw new Error("Unknown fixture task status");
      // A state patch can precede the notification that supplies the final output.
      yield {
        type: "system",
        subtype: "task_updated",
        task_id: "native-1",
        patch: { status: status === "stopped" ? "killed" : status },
        uuid: randomUUID(),
      };
      yield {
        type: "system",
        subtype: "task_notification",
        task_id: "native-1",
        status,
        output_file: path.join(cwd, "native-1.output"),
        summary: "Task " + status,
        uuid: randomUUID(),
      };
      yield {
        type: "assistant",
        uuid: randomUUID(),
        message: {
          id: "wake-message",
          model: "claude-opus-5-5",
          usage,
          content: [{ type: "text", text: "Native task finished" }],
        },
      };
      yield {
        type: "result",
        subtype: "success",
        usage,
        result: "Native task finished",
      };
      await new Promise<void>((resolve) =>
        abort.signal.addEventListener("abort", () => resolve(), { once: true }),
      );
    },
  };
}
