import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import type { Options } from "@anthropic-ai/claude-agent-sdk";
import type {
  SessionFixtureQuery,
  SessionFixtureMessage,
} from "./session-sdk-types.ts";

export type IdentityQueryRecord = Pick<Options, "resume" | "sessionId">;
export const resolveSettings = async () => ({ effective: {} });
export const tool = (
  name: string,
  description: string,
  schema: unknown,
  call: unknown,
) => ({ name, description, schema, call });
export const createSdkMcpServer = (value: unknown) => value;
export const forkSession = async () => ({ sessionId: "unused" });
export const getSessionMessages = async () => [];
const newId =
  process.env.IDENTITY_NEW_SESSION_ID ||
  (() => {
    throw new Error("Expected fixture session identity");
  })();
const conversationId =
  process.env.IDENTITY_CONVERSATION_ID ||
  (() => {
    throw new Error("Expected fixture conversation identity");
  })();
const phase = process.env.IDENTITY_PHASE;
let fixtureCwd: string | undefined;
if (phase) {
  const append = fsp.appendFile;
  fsp.appendFile = async (file, data, ...args) => {
    const text = String(data);
    const transition =
      String(file).includes("/sessions/") &&
      text.includes(newId) &&
      (text.includes("nativeId") || text.includes("pendingNativeSession"));
    const crash =
      fixtureCwd &&
      transition &&
      !fs.existsSync(fixtureCwd + "/.crashed") &&
      (phase !== "committed" || text.includes('"nativeId"'));
    const kill = async () => {
      if (!fixtureCwd) throw new Error("Expected fixture project folder");
      fs.writeFileSync(fixtureCwd + "/.crashed", phase);
      await new Promise<void>((resolve) =>
        process.stdout.write(
          JSON.stringify({ method: "fixture/crash", params: {} }) + "\n",
          () => resolve(),
        ),
      );
      process.kill(process.pid, "SIGKILL");
    };
    if (crash && phase === "before") await kill();
    const result = await append(file, data, ...args);
    if (crash && phase !== "before") {
      if (phase === "missing target") {
        const directory = path.join(
          process.env.CLAUDE_CONFIG_DIR || "",
          "projects",
          (fixtureCwd || "").replace(/[^a-zA-Z0-9]/g, "-"),
        );
        fs.rmSync(path.join(directory, newId + ".jsonl"), { force: true });
      }
      await kill();
    }
    return result;
  };
}
export function query({ prompt, options }: SessionFixtureQuery) {
  const abort = new AbortController();
  fixtureCwd = options.cwd;
  const record: IdentityQueryRecord = {
    ...(options.resume !== undefined ? { resume: options.resume } : {}),
    ...(options.sessionId !== undefined
      ? { sessionId: options.sessionId }
      : {}),
  };
  fs.appendFileSync(options.cwd + "/.queries", JSON.stringify(record) + "\n");
  process.stdout.write(
    JSON.stringify({ method: "fixture/query", params: record }) + "\n",
  );
  return {
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "Claude Max",
      apiProvider: "firstParty",
    }),
    supportedModels: async () => [],
    initializationResult: async () => ({ commands: [] }),
    close() {
      abort.abort();
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<SessionFixtureMessage> {
      const input = await prompt.next();
      if (input.done) return;
      if (phase) {
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
        const file = path.join(
          process.env.CLAUDE_CONFIG_DIR || "",
          "projects",
          options.cwd.replace(/[^a-zA-Z0-9]/g, "-"),
          newId + ".jsonl",
        );
        fs.mkdirSync(path.dirname(file), { recursive: true });
        fs.writeFileSync(
          file,
          JSON.stringify({
            type: "user",
            uuid: "44444444-4444-4444-8444-444444444444",
            parentUuid: null,
            sessionId: newId,
            cwd: options.cwd,
            isSidechain: false,
            timestamp: new Date().toISOString(),
            message: { role: "user", content: "new session anchor" },
          }) + "\n",
        );
      }
      yield {
        type: "system",
        // @ts-expect-error conversation_reset is deliberately unsupported by the SDK; it must not become a resumable session identity.
        subtype: "conversation_reset",
        new_conversation_id: conversationId,
      };
      if (
        process.env.IDENTITY_SOURCE !== "init" &&
        process.env.IDENTITY_SOURCE === "hook"
      )
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
      if (
        process.env.IDENTITY_SOURCE !== "init" &&
        options.hooks?.SessionStart
      ) {
        for (const group of options.hooks.SessionStart)
          for (const hook of group.hooks)
            await hook(
              {
                hook_event_name: "SessionStart",
                source:
                  process.env.IDENTITY_SOURCE === "fork" ? "fork" : "clear",
                session_id: newId,
                cwd: options.cwd,
                transcript_path: "fixture.jsonl",
              },
              undefined,
              { signal: abort.signal },
            );
      }
      if (
        process.env.IDENTITY_SOURCE !== "hook" &&
        process.env.IDENTITY_SOURCE !== "fork"
      )
        yield {
          type: "system",
          subtype: "init",
          session_id: newId,
          claude_code_version: "fixture",
        };
      yield {
        type: "result",
        subtype: "success",
        result: "cleared",
        usage: { input_tokens: 0, output_tokens: 0 },
      };
      await new Promise<void>((resolve) => {
        if (abort.signal.aborted) resolve();
        else
          abort.signal.addEventListener("abort", () => resolve(), {
            once: true,
          });
      });
    },
  };
}
