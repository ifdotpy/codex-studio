import type { FixtureTextQuery as FixtureQuery } from "./sdk-fixtures.ts";
import type { SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import type { StopActive } from "./stop-inputs.ts";
import type { SessionFixtureLegacyMessage as SessionFixtureMessage } from "./fixtures/session-sdk-types.ts";
import fsp from "node:fs/promises";
export interface SteerFixtureOptions {
  markInterrupt?: boolean;
  ignoreInterruptMarker?: boolean;
  logClose?: boolean;
  logQueries?: boolean;
  throwClose?: boolean;
  history?: "empty" | "partial" | "all";
  echoPulledPrefix?: boolean;
  holdPulledTail?: boolean;
  backpressureAnswer?: boolean;
  steadyToSteer?: boolean;
  crashOnClear?: boolean;
}
import { waitForFile } from "./sdk-fixtures.ts";

export { resolveSettings } from "./sdk-fixtures.ts";
import fs from "node:fs";
const mark = (file: string) => {
  fs.writeFileSync(file, "");
  process.stdout.write(
    JSON.stringify({ method: "fixture/signal", params: { name: file } }) + "\n",
  );
};
export function installSteerFaults(
  kind: "hold" | "reject-steer" | "reject-start",
  blockReader = false,
) {
  const append = fsp.appendFile;
  fsp.appendFile = async (file, data, ...args) => {
    const cwd = process.cwd();
    if (kind !== "hold") {
      const marker =
        cwd + (kind === "reject-steer" ? "/.reject-steer" : "/.reject-start");
      const identity =
        kind === "reject-steer" ? "rejected-guard-steer" : "rejected-clear";
      if (
        fs.existsSync(marker) &&
        String(file).includes("/sessions/") &&
        String(data).includes(identity)
      ) {
        fs.rmSync(marker);
        throw new Error(
          kind === "reject-steer"
            ? "steer save denied"
            : "initial clear save denied",
        );
      }
    }
    const value = await append(file, data, ...args);
    if (
      kind === "hold" &&
      fs.existsSync(cwd + "/.hold-final-save") &&
      String(data).includes("completed") &&
      !fs.existsSync(cwd + "/.final-save-held")
    ) {
      mark(cwd + "/.final-save-held");
      await waitForFile(cwd + "/.release-final-save");
    }
    return value;
  };
  if (blockReader)
    Object.defineProperty(process.stdout, "writableLength", {
      configurable: true,
      get() {
        const cwd = process.cwd();
        if (fs.existsSync(cwd + "/.block-reader")) {
          mark(cwd + "/.reader-blocked");
          return 9 * 1024 * 1024;
        }
        return 0;
      },
    });
}
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";
const until = (file: string, signal: AbortSignal) =>
  waitForFile(file, { signal }).catch(() => {});
const usage = { input_tokens: 10, output_tokens: 2 };
export function fixtureQuery(
  { prompt, options }: FixtureQuery,
  config: SteerFixtureOptions = {},
) {
  const abort = new AbortController(),
    cwd = options.cwd;
  const nativeQueue: SDKUserMessage[] = [];
  let stopTurn: (() => void) | null = null;
  let queueMode = false,
    survive = false,
    idle = false;
  let historyParent: string | null | undefined = null;
  const directory =
    process.env.CLAUDE_CONFIG_DIR +
    "/projects/" +
    cwd.replace(/[^a-zA-Z0-9]/g, "-");
  const history =
    directory + "/" + (options.sessionId || options.resume) + ".jsonl";
  if (config.history) {
    fs.mkdirSync(directory, { recursive: true });
    if (!fs.existsSync(history)) fs.writeFileSync(history, "");
  }
  if (config.logQueries || config.history)
    fs.appendFileSync(cwd + "/.queries", "q");
  const read = async () => {
    const input = await prompt.next();
    if (!input.done) {
      fs.appendFileSync(
        cwd + "/.inputs",
        input.value.message.content[0].text + "\n",
      );
      fs.appendFileSync(
        cwd + "/.sdk-inputs",
        JSON.stringify(input.value) + "\n",
      );
      if (
        config.history &&
        (input.value.message.content[0].text === "pulled" ||
          config.history === "all" ||
          (config.history === "partial" && input.value.shouldQuery === false))
      ) {
        fs.appendFileSync(
          history,
          JSON.stringify({
            type: "user",
            uuid: input.value.uuid,
            parentUuid: historyParent,
            sessionId: options.sessionId || options.resume,
            cwd,
            isSidechain: false,
            message: input.value.message,
            timestamp: new Date().toISOString(),
          }) + "\n",
        );
        historyParent = input.value.uuid;
      }
      process.stdout.write(
        JSON.stringify({
          method: "fixture/input",
          params: { text: input.value.message.content[0].text },
        }) + "\n",
      );
    }
    if (
      config.crashOnClear &&
      !input.done &&
      input.value.message.content[0].text === "/clear"
    ) {
      await new Promise<unknown>((resolve) =>
        process.stdout.write(
          JSON.stringify({ method: "fixture/consumed-clear", params: {} }) +
            "\n",
          resolve,
        ),
      );
      process.kill(process.pid, "SIGKILL");
    }
    return input;
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
    stopTask: async (id: string) => {
      fs.appendFileSync(cwd + "/.stopped-tasks", id + "\n");
      if (fs.existsSync(cwd + "/.task-failure"))
        throw new Error("Task stop failed");
    },
    setMcpServers: async () => ({ added: [], removed: [], errors: {} }),
    close() {
      if (
        config.throwClose &&
        fs.existsSync(cwd + "/.throw-close") &&
        !fs.existsSync(cwd + "/.close-thrown")
      ) {
        fs.writeFileSync(cwd + "/.close-thrown", "");
        throw new Error("injected close failure");
      }
      if (config.logClose || config.history)
        fs.writeFileSync(cwd + "/.closed", "");
      abort.abort();
    },
    interrupt: async (
      options?: Parameters<NonNullable<StopActive["q"]>["interrupt"]>[0],
    ) => {
      if (config.markInterrupt) mark(cwd + "/.interrupt-seen");
      if (idle) return { cancelled: [], still_queued: [] };
      if (
        config.ignoreInterruptMarker &&
        fs.existsSync(cwd + "/.ignore-interrupt")
      )
        return { cancelled: [], still_queued: [] };
      if (!queueMode) {
        abort.abort();
        return;
      }
      fs.writeFileSync(
        cwd + "/.interrupt-options",
        JSON.stringify(options ?? null),
      );
      const cancelled =
        options?.cancelQueued && !survive
          ? nativeQueue.splice(0).map((m) => m.uuid)
          : [];
      const receipt = {
        cancelled,
        still_queued: nativeQueue.map((m) => m.uuid),
      };
      fs.writeFileSync(
        cwd + "/.native-interrupt-receipt",
        JSON.stringify(receipt),
      );
      stopTurn!();
      return receipt;
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      while (!abort.signal.aborted) {
        const input = await read();
        if (input.done) return;
        const text = input.value.message.content[0].text;
        yield { type: "user", isReplay: true, uuid: input.value.uuid };
        if (input.value.shouldQuery === false) {
          const fail = text === "prefix-fail";
          yield {
            type: "result",
            subtype: fail ? "error_during_execution" : "success",
            is_error: fail,
            errors: fail ? ["prefix failed"] : undefined,
            num_turns: 0,
            user_message_uuid: input.value.uuid,
            usage,
          };
          continue;
        }
        yield {
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId,
        };
        if (config.steadyToSteer) mark(cwd + "/.steady-to-steer");
        if (text.startsWith("native-"))
          yield {
            type: "system",
            subtype: "background_tasks_changed",
            tasks: [{ task_id: "background-1" }],
          };
        yield {
          type: "stream_event",
          event: { type: "message_start", message: { id: "message-" + text } },
        };
        if (text.startsWith("native-")) {
          queueMode = true;
          survive = text.endsWith("survivor");
          const stopped = new Promise<void>((r) => (stopTurn = r));
          for (;;) {
            const next = await read();
            nativeQueue.push(next.value!);
            if (
              text !== "native-split-all" ||
              next.value!.shouldQuery !== false
            )
              break;
          }
          const submitted = nativeQueue.map((message) => message.uuid!);
          mark(cwd + "/.native-queued");
          await stopped;
          yield {
            type: "result",
            subtype: "error_during_execution",
            is_error: true,
            usage,
            errors: ["interrupted"],
          };
          if (text === "native-split-all")
            yield {
              type: "result",
              subtype: "success",
              usage,
              user_message_uuids: submitted,
            };
          for (const held of nativeQueue.splice(0)) {
            fs.writeFileSync(cwd + "/.survived", "");
            yield { type: "user", isReplay: true, uuid: held.uuid };
          }
          queueMode = false;
          continue;
        }
        if (text.startsWith("dequeue")) {
          let next = await read();
          if (next.done) return;
          yield {
            type: "result",
            subtype: "success",
            usage,
            result: "first",
            user_message_uuid: input.value.uuid!,
            user_message_uuids: [input.value.uuid!],
          };
          while (next.value.shouldQuery === false) {
            if (text !== "dequeue-split-missing")
              yield {
                type: "result",
                subtype: "success",
                num_turns: 0,
                user_message_uuid: next.value.uuid,
                usage,
              };
            next = await read();
            if (next.done) return;
          }
          yield {
            type: "stream_event",
            event: { type: "message_start", message: { id: "message-steer" } },
          };
          yield {
            type: "result",
            subtype: "success",
            usage,
            result: "second",
            user_message_uuid: next.value.uuid!,
            ...(text === "dequeue"
              ? { user_message_uuids: [next.value.uuid!] }
              : {}),
          };
          mark(cwd + "/.native-queued");
          continue;
        }
        let late = null;
        if (text === "ack") {
          for (;;) {
            const next = await read();
            yield { type: "user", isReplay: true, uuid: next.value!.uuid };
            if (next.value!.shouldQuery !== false) break;
            yield {
              type: "result",
              subtype: "success",
              num_turns: 0,
              user_message_uuid: next.value!.uuid,
              usage,
            };
          }
        }
        if (text === "pulled") {
          const prefix = await read();
          if (config.echoPulledPrefix) {
            yield { type: "user", isReplay: true, uuid: prefix.value!.uuid };
            yield {
              type: "result",
              subtype: "success",
              num_turns: 0,
              user_message_uuid: prefix.value!.uuid,
              usage,
            };
          }
        }
        if (text === "late" || text === "success-late" || text === "grace-echo")
          late = await read();
        await until(cwd + "/.end", abort.signal);
        if (abort.signal.aborted) return;
        const token = fs.readFileSync(cwd + "/.end", "utf8");
        fs.rmSync(cwd + "/.end");
        process.stdout.write(
          JSON.stringify({
            method: "fixture/result-ready",
            params: { token },
          }) + "\n",
        );
        if (text === "late" || fs.existsSync(cwd + "/.fail"))
          yield {
            type: "result",
            subtype: "error_during_execution",
            is_error: true,
            usage,
            errors: ["Claude failed"],
          };
        else
          yield { type: "result", subtype: "success", usage, result: "done" };
        if (config.holdPulledTail && text === "pulled")
          await until(cwd + "/.allow-tail", abort.signal);
        idle = true;
        mark(cwd + "/.result-sent");
        if (text === "unread-result") {
          await until(cwd + "/.release", abort.signal);
          if (config.backpressureAnswer) {
            if (abort.signal.aborted) return;
            yield {
              type: "assistant",
              uuid: "backpressure-answer",
              message: {
                id: "after",
                content: [{ type: "text", text: "after final save" }],
                usage,
              },
            };
            yield {
              type: "result",
              subtype: "success",
              usage,
              result: "continued",
            };
          }
        }
        if (late && text === "late") {
          yield { type: "user", isReplay: true, uuid: late.value!.uuid };
          late = null;
        }
        if (late) {
          await until(cwd + "/.echo", abort.signal);
          if (abort.signal.aborted) return;
          yield { type: "user", isReplay: true, uuid: late.value!.uuid };
          yield {
            type: "stream_event",
            event: { type: "message_start", message: { id: "continued" } },
          };
          await until(cwd + "/.continued", abort.signal);
          if (abort.signal.aborted) return;
          yield {
            type: "result",
            subtype: "success",
            usage,
            result: "continued",
          };
        }
        idle = false;
      }
    },
  };
}

export const query = (args: FixtureQuery) => fixtureQuery(args);
