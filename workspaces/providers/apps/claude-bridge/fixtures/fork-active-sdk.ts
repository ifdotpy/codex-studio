export interface ForkQueryEvent {
  id: string;
  start?: boolean;
  close?: boolean;
}
// This fixture writes and reads its own native journal rows.
function parseNativeFixtureRow(text: string): SessionStoreEntry {
  return JSON.parse(text);
}
export { resolveSettings } from "../sdk-fixtures.ts";
import type {
  SessionFixtureQuery,
  SessionFixtureMessage,
} from "./session-sdk-types.ts";
import type { SessionStoreEntry } from "@anthropic-ai/claude-agent-sdk";
import fs from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";
export const tool = (
  name: string,
  description: string,
  schema: unknown,
  call: unknown,
) => ({ name, description, schema, call });
export const createSdkMcpServer = (value: Record<string, unknown>) => ({
  ...value,
});
const histories = new Map<string | undefined, SessionStoreEntry[]>();
export const getSessionMessages = async (id: string) =>
  structuredClone(histories.get(id) || []);
export { forkSession } from "@anthropic-ai/claude-agent-sdk";
export function query({ prompt, options }: SessionFixtureQuery) {
  const abort = new AbortController(),
    id = options.resume || options.sessionId;
  const history = path.join(
    process.env.CLAUDE_CONFIG_DIR || "",
    "projects",
    options.cwd.replace(/[^a-zA-Z0-9]/g, "-"),
    id + ".jsonl",
  );
  fs.mkdirSync(path.dirname(history), { recursive: true });
  let parent: string | undefined | null = null;
  if (fs.existsSync(history)) {
    const last = fs.readFileSync(history, "utf8").trim().split("\n").at(-1);
    if (last) parent = parseNativeFixtureRow(last).uuid;
  }
  const record = (row: SessionStoreEntry) => {
    fs.appendFileSync(
      history,
      JSON.stringify({
        ...row,
        parentUuid: parent,
        sessionId: id,
        cwd: options.cwd,
        isSidechain: false,
        timestamp: new Date().toISOString(),
      }) + "\n",
    );
    parent = row.uuid;
  };
  const log = (event: Omit<ForkQueryEvent, "id">) =>
    fs.appendFileSync(
      options.cwd + "/.query-log",
      JSON.stringify({ id, ...event }) + "\n",
    );
  if (id) log({ start: true });
  return {
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
      if (id) log({ close: true });
      abort.abort();
    },
    interrupt: async () => abort.abort(),
    async *[Symbol.asyncIterator](): AsyncGenerator<SessionFixtureMessage> {
      while (!abort.signal.aborted) {
        const input = await prompt.next();
        if (input.done) return;
        const content = input.value.message.content;
        const first = Array.isArray(content) ? content[0] : undefined;
        if (!first || first.type !== "text")
          throw new Error("Expected fixture text input");
        const text = first.text;
        const messages = histories.get(id) || [];
        histories.set(id, messages);
        const user: SessionStoreEntry = {
          type: "user",
          uuid: input.value.uuid,
          message: input.value.message,
          parent_tool_use_id: null,
        };
        messages.push(user);
        record(user);
        yield { type: "user", isReplay: true, uuid: input.value.uuid };
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
        if (text === "running") {
          await new Promise<void>((resolve) =>
            abort.signal.addEventListener("abort", () => resolve(), {
              once: true,
            }),
          );
          return;
        }
        const reply = {
          type: "assistant",
          uuid: randomUUID(),
          message: {
            id: randomUUID(),
            role: "assistant",
            content: [{ type: "text", text: "reply" }],
          },
          parent_tool_use_id: null,
        } satisfies SessionFixtureMessage & SessionStoreEntry;
        messages.push(reply);
        record(reply);
        yield reply;
        if (text === "background")
          yield {
            type: "system",
            subtype: "background_tasks_changed",
            tasks: [{ task_id: "bg" }],
          };
        yield {
          type: "result",
          subtype: "success",
          usage: { input_tokens: 10, output_tokens: 2 },
          result: "reply",
        };
      }
    },
  };
}
