import type { SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import { waitForFile as waitForFileSignal } from "./testing/file-signals.ts";
const waitForFile: typeof waitForFileSignal = (file, options) =>
  waitForFileSignal(file, { ...options, childIpc: true });
import type { FixtureQuery } from "./sdk-fixtures.ts";
import { inputText } from "./sdk-fixtures.ts";
import type { SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";

export const resolveSettings = async () => ({ effective: {} });
import fs from "node:fs";
import { randomUUID } from "node:crypto";
import path from "node:path";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const usage = { input_tokens: 10, output_tokens: 2 };
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
export function query({ prompt, options }: FixtureQuery) {
  const abort = new AbortController(),
    cwd = options.cwd;
  const session = options.sessionId || options.resume;
  const configDir = process.env.CLAUDE_CONFIG_DIR;
  if (!configDir) throw new Error("Missing fixture config directory");
  const history = path.join(
    configDir,
    "projects",
    cwd.replace(/[^a-zA-Z0-9]/g, "-"),
    session + ".jsonl",
  );
  let interrupted = () => {},
    wedged = false,
    deaf = false;
  let historyParent: string | null | undefined = null;
  const interrupt = new Promise<void>((r) => (interrupted = r));
  const queued: SDKUserMessage[] = [];
  let reading: ReturnType<typeof prompt.next> | null = null;
  const read = () =>
    (reading ||= prompt.next().then((input) => {
      reading = null;
      if (!input.done)
        fs.appendFileSync(cwd + "/.inputs", inputText(input.value) + "\n");
      return input;
    }));
  const tool = {
    type: "tool_use",
    id: "toolu_stall",
    name: "Bash",
    input: { command: "kubectl get cc", timeout: 100 },
  } satisfies import("@anthropic-ai/claude-agent-sdk").SDKAssistantMessage["message"]["content"][number];
  // Hold a steer like the CLI queue: no echo until the turn ends.
  const hold = (value: SDKUserMessage) => {
    queued.push(value);
    if (inputText(value)?.includes("recorded")) {
      const uuid = randomUUID();
      fs.appendFileSync(
        history,
        JSON.stringify({
          type: "attachment",
          uuid,
          parentUuid: historyParent,
          sessionId: session,
          cwd,
          isSidechain: false,
          attachment: { type: "queued_command", source_uuid: value.uuid },
        }) + "\n",
      );
      historyParent = uuid;
    }
  };
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
    close() {
      if (!deaf) abort.abort();
    },
    interrupt() {
      fs.appendFileSync(cwd + "/.interrupts", "x");
      if (wedged) {
        if (fs.existsSync(cwd + "/.stop-now"))
          return Promise.resolve({ still_queued: queued.map((q) => q.uuid) });
        return new Promise(() => {});
      }
      interrupted();
      return Promise.resolve({ still_queued: queued.map((q) => q.uuid) });
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      fs.mkdirSync(path.dirname(history), { recursive: true });
      while (!abort.signal.aborted) {
        const input = await read();
        if (input.done) return;
        const text = inputText(input.value);
        fs.appendFileSync(
          history,
          JSON.stringify({
            type: "user",
            uuid: input.value.uuid,
            parentUuid: historyParent,
            sessionId: session,
            cwd,
            isSidechain: false,
            message: input.value.message,
            timestamp: new Date().toISOString(),
          }) + "\n",
        );
        historyParent = input.value.uuid;
        if (text === "stall") fs.appendFileSync(history, "broken native row\n");
        yield { type: "user", isReplay: true, uuid: input.value.uuid };
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
        yield {
          type: "stream_event",
          event: { type: "message_start", message: { id: "message-" + text } },
        };
        // While the test keeps a .hold file, steers queue before the tool call.
        while (fs.existsSync(cwd + "/.hold")) {
          const next = await Promise.race([
            read(),
            waitForFile(cwd + "/.hold", { absent: true }).then(() => null),
          ]);
          if (next?.done) return;
          if (next) hold(next.value);
        }
        if (text === "answer") {
          yield {
            type: "assistant",
            uuid: randomUUID(),
            message: {
              id: "message-" + text,
              content: [{ type: "text", text: "done" }],
              usage,
            },
          };
          yield { type: "result", subtype: "success", usage, result: "done" };
          continue;
        }
        // The CLI sends its permission request beside the stream, as the SDK
        // control channel does; it does not wait for the bridge to read the tool call.
        const asked =
          text === "approval"
            ? options.canUseTool?.("Bash", tool.input, {
                signal: abort.signal,
                toolUseID: tool.id,
                requestId: tool.id,
                decisionReason: "[1mrm on a critical path[0m",
              })
            : null;
        asked?.catch(() => {});
        yield {
          type: "assistant",
          uuid: randomUUID(),
          message: { id: "message-" + text, content: [tool], usage },
        };
        if (text === "heartbeat") {
          // Progress every 50 ms for the timeout plus twice the stall grace: past
          // the stall budget and the watchdog check after it, so only the progress
          // events keep the call alive.
          const until =
            Date.now() +
            tool.input.timeout +
            2 * Number(process.env.STUDIO_CLAUDE_TOOL_STALL_GRACE_MS);
          for (let i = 0; Date.now() < until; i++) {
            await sleep(50);
            yield {
              type: "tool_progress",
              tool_use_id: tool.id,
              elapsed_time_seconds: i,
            };
          }
          yield {
            type: "user",
            message: {
              role: "user",
              content: [
                { type: "tool_result", tool_use_id: tool.id, content: "ok" },
              ],
            },
          };
          yield { type: "result", subtype: "success", usage, result: "done" };
          continue;
        }
        if (text === "approval") {
          const decision = await asked;
          if (!decision) throw new Error("Missing fixture permission decision");
          fs.writeFileSync(cwd + "/.approved", decision.behavior);
          if (decision.behavior === "deny") {
            yield {
              type: "user",
              message: {
                role: "user",
                content: [
                  {
                    type: "tool_result",
                    tool_use_id: tool.id,
                    is_error: true,
                    content: decision.message,
                  },
                ],
              },
            };
            yield {
              type: "result",
              subtype: "success",
              usage,
              result: "declined",
            };
            continue;
          }
        }
        wedged = text === "wedged" || text === "deaf";
        deaf = text === "deaf";
        for (;;) {
          const next = await Promise.race([
            read(),
            interrupt.then(() => null),
            new Promise<null>((r) =>
              abort.signal.addEventListener("abort", () => r(null), {
                once: true,
              }),
            ),
          ]);
          if (abort.signal.aborted) return;
          if (!next) break;
          if (next.done) return;
          hold(next.value);
        }
        yield {
          type: "result",
          subtype: "error_during_execution",
          is_error: true,
          usage,
          errors: ["interrupted"],
        };
        // Claude Code runs the messages that survived the interrupt next.
        for (const value of queued.splice(0)) {
          yield { type: "user", isReplay: true, uuid: value.uuid };
          yield {
            type: "stream_event",
            event: {
              type: "message_start",
              message: { id: "message-after-" + value.uuid },
            },
          };
          yield {
            type: "result",
            subtype: "success",
            usage,
            result: "answered",
          };
        }
      }
    },
  };
}
