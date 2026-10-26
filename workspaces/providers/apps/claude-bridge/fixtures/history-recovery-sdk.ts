export { resolveSettings } from "../sdk-fixtures.ts";
import type {
  SessionFixtureQuery,
  SessionFixtureMessage,
} from "./session-sdk-types.ts";
import type { SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import fs from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";
export const tool = (
  name: string,
  description: string,
  schema: unknown,
  call: unknown,
) => ({ name, description, schema, call });
export const createSdkMcpServer = (v: Record<string, unknown>) => v;
export const forkSession = async () => {
  throw new Error("unexpected fork");
};
export function query({ prompt, options }: SessionFixtureQuery) {
  const abort = new AbortController(),
    sessionId = options.sessionId || options.resume,
    cwd = options.cwd;
  const file = path.join(
    process.env.CLAUDE_CONFIG_DIR || "",
    "projects",
    cwd.replace(/[^a-zA-Z0-9]/g, "-"),
    sessionId + ".jsonl",
  );
  fs.mkdirSync(path.dirname(file), { recursive: true });
  let parent: string | null | undefined = null;
  const record = (input: SDKUserMessage) => {
    const row = {
      type: "user",
      uuid: input.uuid,
      parentUuid: parent,
      sessionId,
      cwd,
      isSidechain: false,
      timestamp: new Date().toISOString(),
      message: input.message,
      ...(input.isSynthetic
        ? { isMeta: true, origin: { kind: "unclassified" } }
        : {}),
    };
    fs.appendFileSync(file, JSON.stringify(row) + "\n");
    parent = row.uuid;
  };
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
      const initial = await prompt.next();
      if (initial.done) return;
      record(initial.value);
      yield { type: "system", subtype: "init", session_id: sessionId };
      yield { type: "user", isReplay: true, uuid: initial.value.uuid };
      process.stdout.write(
        JSON.stringify({ method: "fixture/anchor", params: {} }) + "\n",
      );
      const parts: SDKUserMessage[] = [];
      do {
        const next = await prompt.next();
        if (next.done) return;
        parts.push(next.value);
      } while (parts.at(-1)?.shouldQuery === false);
      const damage = process.env.HISTORY_DAMAGE;
      if (damage === "all") for (const part of parts) record(part);
      if (damage === "partial") {
        const first = parts[0];
        if (first) record(first);
      }
      if (damage === "malformed uuid" || damage === "unknown record")
        fs.appendFileSync(
          file,
          JSON.stringify({
            type: damage === "unknown record" ? "future-input" : "assistant",
            uuid: damage === "malformed uuid" ? "malformed" : randomUUID(),
            parentUuid: parent,
            sessionId,
            cwd,
            isSidechain: false,
            message: { role: "assistant", content: [] },
          }) + "\n",
        );
      if (damage === "unknown system")
        fs.appendFileSync(
          file,
          JSON.stringify({
            type: "system",
            subtype: "future-input",
            uuid: randomUUID(),
            parentUuid: parent,
            sessionId,
            cwd,
            isSidechain: false,
          }) + "\n",
        );
      if (damage === "malformed auxiliary")
        fs.appendFileSync(
          file,
          JSON.stringify({
            type: "queue-operation",
            operation: 42,
            sessionId,
          }) + "\n",
        );
      if (damage === "invalid block")
        fs.appendFileSync(
          file,
          JSON.stringify({
            type: "user",
            uuid: randomUUID(),
            parentUuid: parent,
            sessionId,
            cwd,
            isSidechain: false,
            message: { role: "user", content: [{ type: "text", text: 42 }] },
          }) + "\n",
        );
      if (damage === "torn") fs.appendFileSync(file, '{"type":');
      if (damage === "truncated")
        fs.writeFileSync(
          file,
          JSON.stringify({
            type: "user",
            uuid: randomUUID(),
            parentUuid: parent,
            sessionId,
            cwd,
            isSidechain: false,
            message: { role: "user", content: "tail without root" },
          }) + "\n",
        );
      if (damage === "missing") fs.rmSync(file);
      yield {
        type: "result",
        subtype: "success",
        usage: { input_tokens: 0, output_tokens: 0 },
        result: "done",
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
