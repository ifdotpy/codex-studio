import type { FixtureQuery } from "./sdk-fixtures.ts";
import type {
  SDKSystemMessage,
  SDKUserMessageReplay,
  SDKResultSuccess,
  SessionMessage,
} from "@anthropic-ai/claude-agent-sdk";
export { tool, createSdkMcpServer } from "./sdk-fixtures.ts";

import {
  appendFileSync,
  mkdirSync,
  writeFileSync,
  copyFileSync,
} from "node:fs";
import path from "node:path";
const messages = new Map<
  string,
  Pick<SessionMessage, "type" | "uuid" | "message" | "parent_tool_use_id">[]
>();
export const forkSession = async () => {
  throw new Error("first-turn rollback must not fork");
};
export const getSessionMessages = async (id: string) => messages.get(id) || [];
export function query({ prompt, options }: FixtureQuery) {
  const log = (value: unknown) =>
    appendFileSync(
      path.join(options.cwd, "sdk-calls.jsonl"),
      JSON.stringify(value) + "\n",
    );
  log({
    kind: "query",
    enabled: options.enableFileCheckpointing,
    env: options.env.CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING,
    resume: options.resume,
  });
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
    rewindFiles: async (messageId: string, rewind: { dryRun?: boolean }) => {
      log({ kind: "rewind", messageId, rewind, resume: options.resume });
      if (!rewind.dryRun)
        copyFileSync(
          path.join(
            process.env.CLAUDE_CONFIG_DIR,
            "file-history",
            options.resume ?? "",
            "0123456789abcdef@v1",
          ),
          path.join(options.cwd, "file.txt"),
        );
      return { canRewind: true, filesChanged: ["file.txt"] };
    },
    close() {
      abort.abort();
    },
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator]() {
      const input = await prompt.next();
      if (input.done) return;
      messages.set(options.sessionId ?? "", [
        {
          type: "user",
          uuid: input.value.uuid,
          message: input.value.message,
          parent_tool_use_id: null,
        },
      ]);
      const config = process.env.CLAUDE_CONFIG_DIR;
      const project = path.join(
        config,
        "projects",
        options.cwd.replace(/[^a-zA-Z0-9]/g, "-"),
      );
      mkdirSync(project, { recursive: true });
      writeFileSync(
        path.join(project, options.sessionId + ".jsonl"),
        JSON.stringify({
          type: "user",
          uuid: input.value.uuid,
          parentUuid: null,
          sessionId: options.sessionId,
          cwd: options.cwd,
          isSidechain: false,
          timestamp: new Date().toISOString(),
          message: input.value.message,
        }) + "\n",
      );
      mkdirSync(path.join(config, "file-history", options.sessionId ?? ""), {
        recursive: true,
      });
      writeFileSync(
        path.join(
          config,
          "file-history",
          options.sessionId,
          "0123456789abcdef@v1",
        ),
        "checkpoint",
      );
      writeFileSync(path.join(options.cwd, "file.txt"), "later user edit");
      appendFileSync(
        path.join(project, options.sessionId + ".jsonl"),
        JSON.stringify({
          type: "file-history-snapshot",
          messageId: input.value.uuid,
          snapshot: {
            messageId: input.value.uuid,
            trackedFileBackups: {
              "file.txt": { backupFileName: "0123456789abcdef@v1", version: 1 },
            },
          },
        }) + "\n",
      );
      log({ kind: "input", messageId: input.value.uuid });
      yield {
        type: "system",
        subtype: "init",
        claude_code_version: "fixture",
        session_id: options.resume || options.sessionId || "",
      } satisfies Pick<
        SDKSystemMessage,
        "type" | "subtype" | "claude_code_version" | "session_id"
      >;
      yield {
        type: "user",
        isReplay: true,
        uuid: input.value.uuid,
      } satisfies Pick<SDKUserMessageReplay, "type" | "isReplay" | "uuid">;
      yield {
        type: "result",
        subtype: "success",
        usage: { input_tokens: 1, output_tokens: 1 },
        result: "done",
      } satisfies Pick<SDKResultSuccess, "type" | "subtype" | "result"> & {
        usage: Pick<
          SDKResultSuccess["usage"],
          "input_tokens" | "output_tokens"
        >;
      };
      await new Promise<Event>((resolve) =>
        abort.signal.addEventListener("abort", resolve, { once: true }),
      );
    },
  };
}
