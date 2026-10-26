// Supplied by the bridge harness so waits retain the fixture process IPC lifetime.
declare const waitForFile: typeof import("./testing/file-signals.ts").waitForFile;
import { inputText } from "./sdk-fixtures.ts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
import fs from "node:fs/promises";
import {
  events,
  modelUsage,
  sdkResult,
  zeroResult,
  assistant,
  interruptedDelta,
} from "./usage-fixtures.ts";
export function query({ prompt, options }: FixtureQuery) {
  const abort = new AbortController();
  let delayedClose = false,
    stuckClose = false,
    reportCloseTiming = false;
  let closeAt: number | undefined;
  let restoreAppend: (() => void) | undefined;
  process.once("exit", () => {
    if (reportCloseTiming)
      process.stdout.write(
        JSON.stringify({
          method: "fixture/shutdownTiming",
          params: { durationMs: Date.now() - (closeAt ?? Date.now()) },
        }) + "\n",
      );
  });
  const result = (zero = false) =>
    zero
      ? zeroResult
      : { ...sdkResult, modelUsage: { "claude-opus-5-5": modelUsage } };
  const stream = (
    event: (typeof events)[number],
    parent: string | null = null,
  ) => ({ type: "stream_event", event, parent_tool_use_id: parent });
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
      restoreAppend?.();
      closeAt ??= Date.now();
      if (stuckClose) return;
      if (delayedClose) setTimeout(() => abort.abort(), 20);
      else abort.abort();
    },
    interrupt: async () => {
      process.stdout.write(
        JSON.stringify({ method: "fixture/interrupt", params: {} }) + "\n",
      );
      abort.abort();
    },
    async *[Symbol.asyncIterator]() {
      while (!abort.signal.aborted) {
        const input = await prompt.next();
        if (input.done) return;
        const text = inputText(input.value);
        yield { type: "user", isReplay: true, uuid: input.value.uuid };
        yield { type: "system", subtype: "init" };
        if (
          text === "shutdown-watchdog" ||
          text === "shutdown-watchdog-inflight"
        ) {
          stuckClose = true;
          if (text === "shutdown-watchdog-inflight") {
            const append = fs.appendFile;
            let release = () => {};
            const released = new Promise<void>(
              (resolve) => (release = resolve),
            );
            restoreAppend = () => {
              fs.appendFile = append;
              release();
            };
            fs.appendFile = async (...args) => {
              if (String(args[1]).includes("stall:watch-tool")) {
                process.stdout.write(
                  JSON.stringify({
                    method: "fixture/stallSaveBlocked",
                    params: {},
                  }) + "\n",
                );
                await released;
              }
              return append(...args);
            };
          }
          yield stream(events[0]);
          yield {
            type: "assistant",
            uuid: "tool-frame",
            message: {
              id: "tool-frame",
              content: [
                {
                  type: "tool_use",
                  id: "watch-tool",
                  name: "Read",
                  input: { file_path: "x" },
                },
              ],
            },
          };
          await new Promise(() => {});
        }
        if (text === "background-stuck") {
          stuckClose = true;
          yield stream(events[0], "background-tool");
          yield result();
          await new Promise(() => {});
        }
        if (text === "shutdown-deferred") {
          stuckClose = true;
          yield stream(events[0]);
          await waitForFile(options.cwd + "/.finish", { signal: abort.signal });
          yield result();
          await new Promise(() => {});
        }
        if (text === "shutdown-error" || text === "shutdown-persist") {
          yield stream(events[0]);
          if (text === "shutdown-persist") {
            const append = fs.appendFile,
              truncate = fs.truncate;
            restoreAppend = () => {
              fs.appendFile = append;
              fs.truncate = truncate;
            };
            // The journal repair is the last asynchronous step before rejection.
            // Its next check-phase callback runs after the entire rejection chain,
            // including persistLater's failure recording, has settled.
            fs.truncate = async (...args) => {
              const value = await truncate(...args);
              setImmediate(() =>
                process.stdout.write(
                  JSON.stringify({
                    method: "fixture/persistFailureSettled",
                    params: {},
                  }) + "\n",
                ),
              );
              return value;
            };
            fs.appendFile = async (...args) => {
              if (String(args[0]).includes("/state/sessions/")) {
                throw new Error("injected persist failure");
              }
              return append(...args);
            };
            yield {
              type: "assistant",
              uuid: "persist-trigger",
              message: {
                id: "persist-trigger",
                content: [{ type: "text", text: "trigger save" }],
              },
            };
          }
          await new Promise<void>((resolve) =>
            abort.signal.addEventListener("abort", () => resolve(), {
              once: true,
            }),
          );
          if (text === "shutdown-persist")
            throw new Error("closed after persist failure");
          yield zeroResult;
          return;
        }
        if (text === "discard-stuck") {
          stuckClose = true;
          yield stream({
            ...events[0],
            message: { ...events[0].message, id: "main-response" },
          });
          yield stream(events[0], "stuck-parent");
          yield {
            type: "assistant",
            parent_tool_use_id: "stuck-parent",
            uuid: "child-started",
            message: {
              id: "child-started",
              content: [{ type: "text", text: "child-started" }],
            },
          };
          await new Promise(() => {});
        }
        if (
          text === "shutdown" ||
          text === "shutdown-stuck" ||
          text === "shutdown-timing" ||
          text === "interrupt"
        ) {
          delayedClose = text === "shutdown";
          stuckClose = text === "shutdown-stuck" || text === "shutdown-timing";
          reportCloseTiming = text === "shutdown-timing";
          yield stream(events[0]);
          await new Promise((resolve) =>
            abort.signal.addEventListener("abort", resolve, { once: true }),
          );
          if (text === "interrupt") yield zeroResult;
          return;
        }
        if (text === "background-owner") {
          yield {
            type: "assistant",
            uuid: "tool-owner",
            message: {
              id: "owner",
              model: "claude-opus-5-5",
              content: [
                {
                  type: "tool_use",
                  id: "background-tool",
                  name: "Task",
                  input: { description: "background" },
                },
              ],
            },
          };
          yield result();
          continue;
        }
        if (text === "background-next") {
          for (const event of events) yield stream(event, "background-tool");
          yield result();
          continue;
        }
        if (text === "windows-alias") {
          yield {
            ...sdkResult,
            modelUsage: {
              "claude-opus-5-5": modelUsage,
              "claude-opus-5-5[1m]": {
                ...modelUsage,
                contextWindow: 1000000,
                canonicalModel: "claude-opus-5-5",
              },
            },
          };
          continue;
        }
        if (text === "windows") {
          yield {
            ...sdkResult,
            modelUsage: {
              "claude-opus-5-5": modelUsage,
              "other-model": { ...modelUsage, contextWindow: 1000000 },
            },
          };
          continue;
        }
        if (text === "background") {
          yield stream(events[0], "background-tool");
          yield result();
          await waitForFile(options.cwd + "/.finish", { signal: abort.signal });
          yield stream(events[1], "background-tool");
          yield stream(events[2], "background-tool");
          yield {
            type: "assistant",
            parent_tool_use_id: "background-tool",
            uuid: "child-done",
            message: {
              id: "response-1",
              content: [{ type: "text", text: "background-finished" }],
            },
          };
          continue;
        }
        if (
          text === "no-stream-then-result" ||
          text === "no-stream-then-close"
        ) {
          yield assistant;
          yield stream(events[0]);
          if (text === "no-stream-then-close") return;
          yield result();
          continue;
        }
        if (text === "no-stream" || text === "no-stream-then-stream") {
          yield assistant;
          yield assistant;
          if (text === "no-stream-then-stream")
            for (const event of events) yield stream(event);
          yield result();
          continue;
        }
        if (text === "null-stop" || text === "absent-stop") {
          yield stream(events[0]);
          const delta = structuredClone(interruptedDelta);
          if (text === "absent-stop") {
            // @ts-expect-error stop_reason is deliberately absent to test an incomplete provider frame.
            delete delta.delta.stop_reason;
          }
          yield stream(delta);
          yield stream(events[2]);
          yield assistant;
          yield result();
          continue;
        }
        if (text === "steer") {
          yield stream(events[0]);
          const next = await prompt.next();
          if (next.done) throw new Error("missing steer fixture input");
          yield result();
          yield { type: "user", isReplay: true, uuid: next.value.uuid };
          yield stream(events[1]);
          yield stream(events[2]);
          await waitForFile(options.cwd + "/.finish", { signal: abort.signal });
          modelUsage.outputTokens = 1440;
          yield result();
          continue;
        }
        if (text !== "result-only" && text !== "zero") {
          yield stream(events[0], text === "subagent" ? "tool-1" : null);
          if (text !== "interrupted" && text !== "query-close") {
            yield {
              type: "assistant",
              uuid: "assistant-1",
              message: {
                id: "response-1",
                model: "claude-opus-5-5",
                usage: { input_tokens: 10, output_tokens: 1 },
                content: [{ type: "text", text: "answer" }],
              },
            };
            yield stream(events[1], text === "subagent" ? "tool-1" : null);
            yield stream(events[2], text === "subagent" ? "tool-1" : null);
          }
        }
        if (text === "query-close") return;
        if (text === "hold-final")
          await waitForFile(options.cwd + "/.finish", { signal: abort.signal });
        yield result(text === "zero");
      }
    },
  };
}
