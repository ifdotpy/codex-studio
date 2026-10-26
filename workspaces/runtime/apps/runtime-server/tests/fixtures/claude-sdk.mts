import fs from "node:fs";
import { randomUUID } from "node:crypto";
import assert from "node:assert/strict";
import type {
  Options,
  SDKUserMessage,
  SDKMessage,
  NonNullableUsage,
  Query,
  AnyZodRawShape,
  SdkMcpToolDefinition,
} from "@anthropic-ai/claude-agent-sdk";
import fsp from "node:fs/promises";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
const { promise: raceStarted, resolve: beginRace } =
  Promise.withResolvers<void>();
let releaseRace = () => {};
const stoppedTasks = new Set();
const originalRead = fsp.readFile.bind(fsp);
fsp.readFile = new Proxy(originalRead, {
  async apply(target, receiver, args: unknown[]) {
    if (String(args[0]).endsWith("/race.png")) {
      const { promise: held, resolve } = Promise.withResolvers<void>();
      releaseRace = resolve;
      beginRace();
      await held;
    }
    return Reflect.apply(target, receiver, args);
  },
});

const appendOriginal = fsp.appendFile.bind(fsp);
fsp.appendFile = async (file, data, ...args) => {
  const root = process.argv[2],
    marker = root + "/held-persist";
  if (
    fs.existsSync(root + "/fail-accepted") &&
    String(data).includes("accepted")
  ) {
    fs.unlinkSync(root + "/fail-accepted");
    throw new Error("Accepted receipt disk failure");
  }
  const value = await appendOriginal(file, data, ...args);
  if (
    String(file).endsWith(".jsonl") &&
    String(data).includes("race-input") &&
    !fs.existsSync(marker)
  ) {
    fs.writeFileSync(marker, "yes");
    if (fs.existsSync(root + "/crash-mode")) await new Promise(() => {});
    while (!fs.existsSync(root + "/release-persist"))
      await new Promise((r) => setTimeout(r, 5));
  }
  return value;
};

const renameOriginal = fsp.rename.bind(fsp);
fsp.rename = async (from, to) => {
  const value = await renameOriginal(from, to),
    root = process.argv[2];
  if (
    String(to).endsWith(".meta.json") &&
    fs.existsSync(root + "/gate-persist") &&
    !fs.existsSync(root + "/held-persist")
  ) {
    fs.writeFileSync(root + "/held-persist", "yes");
    while (!fs.existsSync(root + "/release-persist"))
      await new Promise((r) => setTimeout(r, 5));
  }
  return value;
};

export const tool = <Schema extends AnyZodRawShape>(
  name: string,
  description: string,
  schema: Schema,
  call: SdkMcpToolDefinition<Schema>["handler"],
) => ({
  name,
  description,
  schema,
  call,
});
export const createSdkMcpServer = (value: {
  name: string;
  version: string;
  tools: ReturnType<typeof tool>[];
}) => {
  const instance = new McpServer({ name: value.name, version: value.version });
  for (const t of value.tools)
    instance.registerTool(
      t.name,
      { description: t.description, inputSchema: t.schema },
      t.call,
    );
  return { ...value, instance };
};
export const forkSession = async () => ({
  sessionId: "22222222-2222-4222-8222-222222222222",
});
export const getSessionMessages = async () => [];
// No user or project settings participate in the scripted bridge fixture.
// Match the SDK's async ResolvedSettings envelope consumed by the bridge.
export const resolveSettings = async () => ({
  effective: {},
  provenance: {},
  sources: [],
});
type Assistant = Extract<SDKMessage, { type: "assistant" }>;
type Message = Assistant["message"];
type TextBlock = Extract<Message["content"][number], { type: "text" }>;
type FixtureMessage = Omit<Partial<Message>, "content" | "usage"> & {
  content?: (
    | Omit<TextBlock, "citations">
    | Exclude<Message["content"][number], TextBlock>
  )[];
  usage?: Partial<Message["usage"]>;
};
type Result = Extract<SDKMessage, { type: "result"; subtype: "success" }>;
type FixtureFrame =
  | (Omit<Partial<Assistant>, "type" | "message"> & {
      type: "assistant";
      message: FixtureMessage;
    })
  | (Omit<Partial<Result>, "type" | "subtype" | "usage"> & {
      type: "result";
      subtype: "success";
      usage?: Partial<Result["usage"]>;
    })
  | { type: "result"; subtype: "error"; errors: string[] }
  | (Partial<Extract<SDKMessage, { type: "system"; subtype: "init" }>> & {
      type: "system";
      subtype: "init";
    })
  | (Partial<
      Extract<SDKMessage, { type: "system"; subtype: "local_command_output" }>
    > & { type: "system"; subtype: "local_command_output" })
  | (Partial<
      Extract<SDKMessage, { type: "system"; subtype: "task_started" }>
    > & { type: "system"; subtype: "task_started" })
  | (Omit<
      Partial<
        Extract<SDKMessage, { type: "system"; subtype: "task_notification" }>
      >,
      "status"
    > & {
      type: "system";
      subtype: "task_notification";
      status: "completed" | "killed";
    })
  | (Partial<Extract<SDKMessage, { type: "rate_limit_event" }>> & {
      type: "rate_limit_event";
    })
  | {
      type: "stream_event";
      event:
        | { type: "message_start"; message: FixtureMessage }
        | {
            type: "content_block_delta";
            index?: number;
            delta: { type: "text_delta"; text: string };
          };
    }
  | (Omit<Partial<SDKUserMessage>, "message"> & {
      type: "user";
      isReplay?: boolean;
      message?: Partial<SDKUserMessage["message"]>;
    });
const fixtureSession = "22222222-2222-4222-8222-222222222222";
function usage(input?: Partial<NonNullableUsage>): NonNullableUsage;
function usage(input?: Partial<Message["usage"]>): Message["usage"];
function usage(input?: Partial<Message["usage"]>): Message["usage"] {
  return {
    input_tokens: 0,
    output_tokens: 0,
    fallback_credit: null,
    iterations: [],
    speed: "standard",
    output_tokens_details: { thinking_tokens: 0 },
    cache_creation_input_tokens: 0,
    cache_read_input_tokens: 0,
    cache_creation: {
      ephemeral_5m_input_tokens: 0,
      ephemeral_1h_input_tokens: 0,
    },
    server_tool_use: { web_fetch_requests: 0, web_search_requests: 0 },
    service_tier: "standard",
    inference_geo: "not_available",
    ...input,
  };
}
function message(input: FixtureMessage): Message {
  return {
    id: "fixture-message",
    type: "message",
    role: "assistant",
    model: "fixture-model",
    stop_reason: null,
    stop_details: null,
    stop_sequence: null,
    container: null,
    diagnostics: null,
    context_management: null,
    ...input,
    content: (input.content ?? []).map((block) =>
      block.type === "text" ? { citations: [], ...block } : block,
    ),
    usage: usage(input.usage),
  };
}
/** Required SDK fields have one fixture default; scenario fields remain explicit. */
function sdkMessage(input: FixtureFrame): SDKMessage {
  const identity = { uuid: randomUUID(), session_id: fixtureSession };
  switch (input.type) {
    case "assistant":
      return {
        ...identity,
        parent_tool_use_id: null,
        ...input,
        message: message(input.message),
      };
    case "stream_event":
      return {
        ...identity,
        parent_tool_use_id: null,
        type: "stream_event",
        event:
          input.event.type === "message_start"
            ? { ...input.event, message: message(input.event.message) }
            : { index: 0, ...input.event },
      };
    case "rate_limit_event":
      return {
        ...identity,
        rate_limit_info: { status: "allowed" as const },
        ...input,
      };
    case "user":
      return {
        ...identity,
        parent_tool_use_id: null,
        ...input,
        message: { role: "user", content: [], ...input.message },
      };
    case "system":
      switch (input.subtype) {
        case "init":
          return {
            ...identity,
            apiKeySource: "none" as const,
            claude_code_version: "fixture",
            cwd: "/fixture",
            tools: [],
            mcp_servers: [],
            model: "fixture-model",
            permissionMode: "default" as const,
            slash_commands: [],
            output_style: "default",
            skills: [],
            plugins: [],
            ...input,
          };
        case "local_command_output":
          return { ...identity, content: "", ...input };
        case "task_started":
          return {
            ...identity,
            task_id: "worker-1",
            description: "Worker",
            ...input,
          };
        case "task_notification":
          if (input.status === "killed") {
            // @ts-expect-error The legacy protocol witness deliberately emits killed instead of the SDK's stopped status.
            return {
              ...identity,
              task_id: "worker-1",
              output_file: "",
              summary: "",
              ...input,
            };
          }
          return {
            ...identity,
            task_id: "worker-1",
            output_file: "",
            summary: "",
            ...input,
            status: input.status,
          };
      }
    case "result":
      if (input.subtype === "error") {
        // @ts-expect-error The legacy limit witness deliberately emits the pre-SDK error subtype.
        return input;
      }
      return {
        ...identity,
        duration_ms: 0,
        duration_api_ms: 0,
        is_error: false,
        num_turns: 1,
        result: "",
        stop_reason: null,
        total_cost_usd: 0,
        modelUsage: {},
        permission_denials: [],
        ...input,
        usage: usage(input.usage),
      };
  }
}

type PromptEvent = {
  kind: "prompt";
  result: IteratorResult<SDKUserMessage, void>;
};
type BackgroundEvent = {
  kind: "background";
  taskId: string;
  status: "killed" | "completed";
};
type FixtureOptions = Omit<Options, "mcpServers"> & {
  mcpServers?: Record<string, ReturnType<typeof createSdkMcpServer>>;
};
export function query({
  prompt,
  options,
}: {
  prompt: AsyncIterator<SDKUserMessage, void>;
  options: FixtureOptions;
}) {
  if (
    options.systemPrompt &&
    !(options.disallowedTools || []).includes("Agent")
  )
    throw new Error("Native subagents must stay disabled");
  if (options.systemPrompt && options.perTaskStopAffordance !== true)
    throw new Error(
      "Per-task stop control must preserve other background tasks",
    );
  let abort = new AbortController();
  let turnAbort: AbortController | undefined;
  let backgroundTasks = new Map<string, boolean>();
  let outputTotal = 0;
  if (options.env?.TMPDIR)
    fs.writeFileSync(
      options.cwd + "/.probe-options",
      JSON.stringify({
        permissionMode: options.permissionMode,
        tmpdir: options.env.TMPDIR,
      }),
    );
  if (options.systemPrompt)
    fs.appendFileSync(
      options.cwd + "/.queries",
      JSON.stringify({
        resume: options.resume || null,
        sessionId: options.sessionId || null,
        permissionMode: options.permissionMode,
        systemPrompt: options.systemPrompt,
        tools: options.tools,
        settingSources: options.settingSources,
        strictMcpConfig: options.strictMcpConfig,
        mcpServers: Object.keys(options.mcpServers || {}),
        effort: options.effort,
        settings: options.settings,
      }) + "\n",
    );
  if (options.systemPrompt)
    fs.appendFileSync(
      options.cwd + "/.thinking-flags",
      JSON.stringify({
        phase: "initial",
        model: options.model,
        settings: options.settings,
      }) + "\n",
    );
  return {
    mcpServerStatus: async () =>
      fs.existsSync(options.cwd + "/.mcp-status.json")
        ? JSON.parse(fs.readFileSync(options.cwd + "/.mcp-status.json", "utf8"))
        : [],
    reconnectMcpServer: async () => {},
    supportedModels: async () => [
      {
        value: "default",
        displayName: "Default",
        resolvedModel: "claude-opus-5-5",
        supportsEffort: true,
        supportedEffortLevels: ["low", "medium", "high"],
      },
      { value: "opus[1m]", resolvedModel: "claude-opus-5-5" },
      { value: "sonnet", resolvedModel: "claude-sonnet-5" },
    ],
    accountInfo: async () => {
      if (fs.existsSync(options.cwd + "/.hang-account"))
        return new Promise(() => {});
      return {
        email: fs.existsSync(options.cwd + "/.wrong-account")
          ? "different@example.test"
          : "test@example.test",
        subscriptionType: "Claude Max",
        apiProvider: "firstParty",
      };
    },
    initializationResult: async () => ({
      commands: [{ name: "compact", description: "Compact history" }],
    }),
    usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET: async () => ({
      rate_limits_available: true,
      subscription_type: "max",
      rate_limits: {
        five_hour: { utilization: 11, resets_at: "2026-09-22T08:00:00Z" },
        seven_day: { utilization: 4 },
        model_scoped: [{ display_name: "Fable", utilization: 7 }],
      },
    }),
    setModel: async (model: Parameters<Query["setModel"]>[0]) => {
      if (model === "reject-model") throw new Error("Native model rejected");
      if (fs.existsSync(options.cwd + "/.dead-query"))
        throw new Error("Claude Code process aborted by user");
      if (fs.existsSync(options.cwd + "/.hang-preparation")) {
        fs.writeFileSync(options.cwd + "/.preparation-entered", "yes");
        await new Promise((resolve) =>
          abort.signal.addEventListener("abort", resolve, { once: true }),
        );
      }
    },
    setPermissionMode: async () => {},
    setMcpServers: async (
      servers: NonNullable<FixtureOptions["mcpServers"]>,
    ) => {
      fs.appendFileSync(
        options.cwd + "/.mcp-sets",
        JSON.stringify(
          Object.keys(servers).map((name) => [
            name,
            servers[name]?.tools.map((t) => t.name),
          ]),
        ) + "\n",
      );
      if (
        !Object.keys(servers).length &&
        fs.existsSync(options.cwd + "/.hang-mcp")
      )
        return new Promise(() => {});
      return { added: [], removed: [], errors: {} };
    },
    applyFlagSettings: async (
      settings: Parameters<Query["applyFlagSettings"]>[0],
    ) => {
      fs.appendFileSync(
        options.cwd + "/.thinking-flags",
        JSON.stringify({ phase: "live", settings }) + "\n",
      );
    },
    stopTask: async (taskId: Parameters<Query["stopTask"]>[0]) => {
      stoppedTasks.add(taskId);
      fs.appendFileSync(options.cwd + "/.stopped-tasks", taskId + "\n");
    },
    close() {
      if (options.systemPrompt)
        fs.appendFileSync(options.cwd + "/.query-closes", "closed\n");
      abort.abort();
      turnAbort?.abort();
    },
    interrupt: async () => {
      turnAbort?.abort();
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<SDKMessage, void, unknown> {
      let transcriptPrefix: string[] = [];
      let pendingPrompt: Promise<PromptEvent> = prompt
        .next()
        .then((result) => ({ kind: "prompt" as const, result }));
      let pendingBackground: Promise<BackgroundEvent> | null = null;
      while (!abort.signal.aborted) {
        if (backgroundTasks.size && !pendingBackground) {
          const [taskId] = backgroundTasks.keys();
          assert.ok(taskId);
          pendingBackground = (async () => {
            while (
              !abort.signal.aborted &&
              !stoppedTasks.has(taskId) &&
              !fs.existsSync(options.cwd + "/.release-background")
            )
              await new Promise((r) => setTimeout(r, 5));
            return {
              kind: "background",
              taskId,
              status: stoppedTasks.has(taskId) ? "killed" : "completed",
            };
          })();
        }
        const event: PromptEvent | BackgroundEvent = await Promise.race(
          pendingBackground
            ? [pendingPrompt, pendingBackground]
            : [pendingPrompt],
        );
        if (event.kind === "background") {
          pendingBackground = null;
          backgroundTasks.delete(event.taskId);
          yield sdkMessage({
            type: "system",
            subtype: "task_notification",
            task_id: event.taskId,
            status: event.status,
          });
          continue;
        }
        const input: IteratorResult<SDKUserMessage, void> = event.result;

        if (input.done) return;
        pendingPrompt = prompt
          .next()
          .then((result) => ({ kind: "prompt", result }));
        if (
          !/^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/i.test(
            input.value.uuid ?? "",
          )
        )
          throw new Error("Native input requires UUID");
        assert.ok(input.value.uuid);
        if (options.systemPrompt)
          fs.appendFileSync(
            options.cwd + "/.sdk-inputs",
            JSON.stringify(input.value) + "\n",
          );
        const content: SDKUserMessage["message"]["content"] =
          input.value.message.content;
        assert.ok(Array.isArray(content) && content[0]?.type === "text");
        let text: string = content[0].text;
        yield sdkMessage({
          type: "user",
          isReplay: true,
          uuid: input.value.uuid,
        });
        // SDK shouldQuery:false appends context for the next querying message;
        // its receipt must not generate or complete an assistant turn.
        if (input.value.shouldQuery === false) {
          transcriptPrefix.push(text);
          yield sdkMessage({
            type: "result",
            subtype: "success",
            num_turns: 0,
            user_message_uuid: input.value.uuid,
          });
          continue;
        }
        text = [...transcriptPrefix, text].join("\n\n");
        transcriptPrefix = [];
        turnAbort = new AbortController();
        const turnSignal = turnAbort.signal;
        yield sdkMessage({
          type: "system",
          subtype: "init",
          session_id: options.resume || options.sessionId || fixtureSession,
        });
        if (text === "steer-race") {
          await raceStarted;
          yield sdkMessage({
            type: "result",
            subtype: "success",
            usage: { input_tokens: 10, output_tokens: 2 },
            result: "Before steer",
          });
          releaseRace();
          continue;
        }
        if (text === "steer") {
          yield sdkMessage({
            type: "stream_event",
            event: { type: "message_start", message: { id: "waiting" } },
          });
          yield sdkMessage({
            type: "stream_event",
            event: {
              type: "content_block_delta",
              delta: { type: "text_delta", text: "Waiting for steer" },
            },
          });
          const next = await pendingPrompt;

          pendingPrompt = prompt
            .next()
            .then((result) => ({ kind: "prompt", result }));
          if (next.kind !== "prompt") throw new Error("Expected a steer input");
          const nextInput = next.result;
          assert.ok(!nextInput.done);
          assert.ok(nextInput.value.uuid);
          if (
            !/^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/i.test(
              nextInput.value?.uuid ?? "",
            )
          )
            throw new Error("Native steer requires UUID");
          yield sdkMessage({
            type: "user",
            isReplay: true,
            uuid: nextInput.value.uuid,
          });
          assert.ok(!nextInput.done);
          const replacement = nextInput.value.message.content;
          assert.ok(
            Array.isArray(replacement) && replacement[0]?.type === "text",
          );
          text = replacement[0].text;
          if (text !== "replacement") throw new Error("Wrong steer");
          while (
            !fs.existsSync(options.cwd + "/.release-steer") &&
            !abort.signal.aborted
          )
            await new Promise((r) => setTimeout(r, 5));
        }
        if (text === "plan") {
          const decision = await options.canUseTool?.(
            "ExitPlanMode",
            { plan: "A visible plan" },
            { toolUseID: "plan", requestId: "plan", signal: turnSignal },
          );
          if (decision?.behavior !== "deny")
            throw new Error("Plan executed without a separate user turn");
        }
        if (text === "permission") {
          const decision = await options.canUseTool?.(
            "Bash",
            { command: "echo hello" },
            { toolUseID: "tool-1", requestId: "tool-1", signal: turnSignal },
          );
          if (decision?.behavior !== "deny") throw new Error("Expected denial");
        }
        if (text === "question") {
          const decision = await options.canUseTool?.(
            "AskUserQuestion",
            {
              questions: [
                {
                  question: "Choose?",
                  header: "Choice",
                  options: [{ label: "A", description: "First" }],
                },
              ],
            },
            { toolUseID: "ask", requestId: "ask", signal: turnSignal },
          );
          if (
            decision?.behavior !== "allow" ||
            !decision.updatedInput ||
            typeof decision.updatedInput.answers !== "object" ||
            !decision.updatedInput.answers ||
            !("Choose?" in decision.updatedInput.answers) ||
            decision.updatedInput.answers["Choose?"] !== "A"
          )
            throw new Error("Missing answer");
        }
        if (text === "question-multi") {
          const decision = await options.canUseTool?.(
            "AskUserQuestion",
            {
              questions: [
                {
                  question: "Features?",
                  header: "Features",
                  multiSelect: true,
                  options: [
                    { label: "A", description: "First" },
                    { label: "B", description: "Second" },
                  ],
                },
              ],
            },
            {
              toolUseID: "ask-multi",
              requestId: "ask-multi",
              signal: turnSignal,
            },
          );
          if (
            decision?.behavior !== "allow" ||
            !decision.updatedInput ||
            typeof decision.updatedInput.answers !== "object" ||
            !decision.updatedInput.answers ||
            !("Features?" in decision.updatedInput.answers) ||
            decision.updatedInput.answers["Features?"] !== "A, B"
          )
            throw new Error("Missing multi answer");
        }
        if (text === "tool") {
          const tool = options.mcpServers?.studio?.tools[0];
          assert.ok(tool);
          const result = await tool.call(
            { text: "hello" },
            { signal: turnSignal },
          );
          if (
            result.content[0]?.type !== "text" ||
            result.content[0].text !== "tool-ok"
          )
            throw new Error("Tool result lost");
        }
        if (text === "tool-pre-aborted") {
          const tool = options.mcpServers?.studio?.tools[0];
          assert.ok(tool);
          const cancelled = new AbortController();
          cancelled.abort();
          await tool.call({ text: "hello" }, { signal: cancelled.signal });
        }
        if (text.startsWith("wait"))
          await new Promise<void>((resolve) => {
            if (turnSignal.aborted) resolve();
            else
              turnSignal.addEventListener("abort", () => resolve(), {
                once: true,
              });
          });
        // Closing the real SDK terminates its stream; it does not generate a
        // successful model response after releasing an aborted wait.
        if (abort.signal.aborted) return;
        if (text === "fail") throw new Error("Native failure");
        if (text === "/compact")
          yield sdkMessage({
            type: "system",
            subtype: "local_command_output",
            // @ts-expect-error Preserve the legacy non-UUID identity exercised by this protocol witness.
            uuid: "compact-result",
            content: "Native compact received",
          });
        if (
          text === "background" ||
          text === "background-hold" ||
          text === "background-stop-hold"
        ) {
          backgroundTasks.set("worker-1", true);
          yield sdkMessage({
            type: "system",
            subtype: "task_started",
            task_id: "worker-1",
            description: "Worker",
            task_type: "agent",
          });
        }
        if (text === "background-stop-hold") {
          fs.writeFileSync(options.cwd + "/.background-waiting", "yes");
          await new Promise<void>((resolve) => {
            if (turnSignal.aborted) resolve();
            else
              turnSignal.addEventListener("abort", () => resolve(), {
                once: true,
              });
          });
          if (stoppedTasks.has("worker-1")) {
            backgroundTasks.delete("worker-1");
            yield sdkMessage({
              type: "system",
              subtype: "task_notification",
              task_id: "worker-1",
              status: "killed",
            });
          }
          yield sdkMessage({
            type: "result",
            subtype: "success",
            usage: { input_tokens: 1, output_tokens: 0 },
            result: "",
          });
          continue;
        }
        if (text === "claude-limit") {
          const resetsAt = Math.floor(Date.now() / 1000) + 3600;
          yield sdkMessage({
            type: "rate_limit_event",
            rate_limit_info: {
              status: "rejected",
              rateLimitType: "five_hour",
              resetsAt,
              utilization: 1,
            },
          });
          yield sdkMessage({
            type: "result",
            subtype: "error",
            errors: [
              "Claude usage limit reached (five_hour). Resets at " +
                new Date(resetsAt * 1000).toISOString() +
                ". The turn waits.",
            ],
          });
          continue;
        }
        if (text === "claude-offline") {
          const offline =
            "API Error: Can't reach the API server \u2014 check your internet or DNS (ENOTFOUND)";
          yield sdkMessage({
            type: "assistant",
            error: "unknown",
            message: {
              id: "offline",
              content: [{ type: "text", text: offline }],
            },
          });
          yield sdkMessage({
            type: "result",
            subtype: "success",
            is_error: true,
            result: offline,
          });
          continue;
        }
        if (text.startsWith("Continue the previous turn"))
          fs.writeFileSync(options.cwd + "/.continued", text);
        if (text === "subagent-text") {
          yield sdkMessage({
            type: "assistant",
            parent_tool_use_id: "agent-tool",
            // @ts-expect-error Preserve the legacy non-UUID identity exercised by this protocol witness.
            uuid: "worker-message",
            message: {
              id: "worker-answer",
              content: [
                { type: "text", text: "First worker paragraph" },
                { type: "text", text: "Second worker paragraph" },
              ],
            },
          });
        }
        if (text === "snapshots") {
          yield sdkMessage({
            type: "system",
            subtype: "local_command_output",
            // @ts-expect-error Preserve the legacy non-UUID identity exercised by this protocol witness.
            uuid: "command-output",
            content: "Native command output",
          });
          yield sdkMessage({
            type: "assistant",
            message: {
              id: "snapshot-only",
              content: [{ type: "text", text: "Snapshot without stream" }],
            },
          });
          yield sdkMessage({
            type: "assistant",
            message: {
              id: "native-tool",
              content: [
                {
                  type: "tool_use",
                  id: "bash-output",
                  name: "Bash",
                  input: { command: "pwd" },
                },
              ],
            },
          });
          yield sdkMessage({
            type: "user",
            message: {
              content: [
                {
                  type: "tool_result",
                  tool_use_id: "bash-output",
                  content: "/work",
                },
              ],
            },
          });
        }
        if (text === "split-blocks") {
          yield sdkMessage({
            type: "assistant",
            uuid: "11111111-1111-4111-8111-111111111111",
            message: {
              id: "split-message",
              content: [{ type: "text", text: "First text block." }],
              stop_reason: null,
            },
          });
          yield sdkMessage({
            type: "assistant",
            uuid: "22222222-2222-4222-8222-222222222222",
            message: {
              id: "split-message",
              content: [{ type: "text", text: "Second text block." }],
              stop_reason: null,
            },
          });
          yield sdkMessage({
            type: "assistant",
            uuid: "22222222-2222-4222-8222-222222222222",
            message: {
              id: "split-message",
              content: [{ type: "text", text: "Second text block." }],
              stop_reason: null,
            },
          });
          yield sdkMessage({
            type: "result",
            subtype: "success",
            usage: { input_tokens: 10, output_tokens: 2 },
            result: "First text block. Second text block.",
          });
          continue;
        }
        if (text === "rate-usage") {
          yield sdkMessage({
            type: "stream_event",
            event: { type: "message_start", message: { id: "rate-1" } },
          });
          yield sdkMessage({
            type: "stream_event",
            event: {
              type: "content_block_delta",
              delta: { type: "text_delta", text: "Rate answer" },
            },
          });
          for (const output of [6, 6, 8])
            yield sdkMessage({
              type: "assistant",
              message: {
                id: "rate-1",
                model: "claude-opus-5-5",
                usage: {
                  input_tokens: 100,
                  cache_read_input_tokens: 900,
                  cache_creation_input_tokens: 50,
                  output_tokens: output,
                },
                content: [{ type: "text", text: "Rate answer" }],
              },
            });
          yield sdkMessage({
            type: "assistant",
            message: {
              id: "rate-2",
              model: "claude-opus-5-5",
              usage: {
                input_tokens: 200,
                cache_read_input_tokens: 1800,
                cache_creation_input_tokens: 100,
                output_tokens: 12,
              },
              content: [{ type: "text", text: "Second answer" }],
            },
          });
          outputTotal += 20;
          yield sdkMessage({
            type: "result",
            subtype: "success",
            usage: { input_tokens: 3000, output_tokens: outputTotal },
            result: "Second answer",
          });
          continue;
        }
        if (text.startsWith("split-stream")) {
          yield sdkMessage({
            type: "stream_event",
            event: { type: "message_start", message: { id: "stream-blocks" } },
          });
          yield sdkMessage({
            type: "stream_event",
            event: {
              type: "content_block_delta",
              index: 0,
              delta: { type: "text_delta", text: "First stream block." },
            },
          });
          yield sdkMessage({
            type: "assistant",
            uuid: "11111111-1111-4111-8111-111111111111",
            message: {
              id: "stream-blocks",
              content: [{ type: "text", text: "First stream block." }],
              stop_reason: null,
            },
          });
          yield sdkMessage({
            type: "stream_event",
            event: {
              type: "content_block_delta",
              index: 1,
              delta: { type: "text_delta", text: "Second stream block." },
            },
          });
          yield sdkMessage({
            type: "assistant",
            uuid: "22222222-2222-4222-8222-222222222222",
            message: {
              id: "stream-blocks",
              content: [{ type: "text", text: "Second stream block." }],
              stop_reason: null,
            },
          });
          yield sdkMessage({
            type: "result",
            subtype: "success",
            usage: { input_tokens: 10, output_tokens: 2 },
            result: "First stream block. Second stream block.",
          });
          continue;
        }
        yield sdkMessage({
          type: "stream_event",
          event: { type: "message_start", message: { id: "answer" } },
        });
        yield sdkMessage({
          type: "stream_event",
          event: {
            type: "content_block_delta",
            delta: { type: "text_delta", text: "Visible answer" },
          },
        });
        yield sdkMessage({
          type: "assistant",
          message: {
            id: "answer",
            content: [
              { type: "text", text: "Visible answer" },
              {
                type: "thinking",
                thinking: "Visible reasoning",
                signature: "PRIVATE_SIGNATURE",
              },
            ],
          },
        });
        yield sdkMessage({
          type: "result",
          subtype: "success",
          usage: { input_tokens: 10, output_tokens: 2 },
          result: "Visible answer",
        });
        if (text === "background") {
          yield sdkMessage({
            type: "assistant",
            parent_tool_use_id: "agent-tool",
            // @ts-expect-error Preserve the legacy non-UUID identity exercised by this protocol witness.
            uuid: "late-worker",
            message: {
              id: "late-answer",
              content: [{ type: "text", text: "Late worker text" }],
            },
          });
          yield sdkMessage({
            type: "system",
            subtype: "task_notification",
            task_id: "worker-1",
            status: "completed",
            summary: "Worker finished",
          });
          backgroundTasks.delete("worker-1");
        }
      }
    },
  };
}
