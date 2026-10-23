import fs from "node:fs";
import fsp from "node:fs/promises";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
let beginRace, releaseRace;
const raceStarted = new Promise((resolve) => (beginRace = resolve));
const stoppedTasks = new Set();
const originalRead = fsp.readFile.bind(fsp);
fsp.readFile = async (file, ...args) => {
  if (String(file).endsWith("/race.png")) {
    const held = new Promise((resolve) => (releaseRace = resolve));
    beginRace();
    await held;
  }
  return originalRead(file, ...args);
};

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

export const tool = (name, description, schema, call) => ({
  name,
  description,
  schema,
  call,
});
export const createSdkMcpServer = (value) => {
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
export function query({ prompt, options }) {
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
  let turnAbort;
  let backgroundTasks = new Map();
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
    setModel: async (model) => {
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
    setMcpServers: async (servers) => {
      fs.appendFileSync(
        options.cwd + "/.mcp-sets",
        JSON.stringify(
          Object.keys(servers).map((name) => [
            name,
            servers[name].tools.map((t) => t.name),
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
    applyFlagSettings: async (settings) => {
      fs.appendFileSync(
        options.cwd + "/.thinking-flags",
        JSON.stringify({ phase: "live", settings }) + "\n",
      );
    },
    stopTask: async (taskId) => {
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
    async *[Symbol.asyncIterator]() {
      let transcriptPrefix = [];
      let pendingPrompt = prompt
        .next()
        .then((result) => ({ kind: "prompt", result }));
      let pendingBackground = null;
      while (!abort.signal.aborted) {
        if (backgroundTasks.size && !pendingBackground) {
          const [taskId] = backgroundTasks.keys();
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
        const event = await Promise.race(
          pendingBackground
            ? [pendingPrompt, pendingBackground]
            : [pendingPrompt],
        );
        if (event.kind === "background") {
          pendingBackground = null;
          backgroundTasks.delete(event.taskId);
          yield {
            type: "system",
            subtype: "task_notification",
            task_id: event.taskId,
            status: event.status,
          };
          continue;
        }
        const input = event.result;
        pendingPrompt = null;
        if (input.done) return;
        pendingPrompt = prompt
          .next()
          .then((result) => ({ kind: "prompt", result }));
        if (
          !/^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/i.test(
            input.value.uuid,
          )
        )
          throw new Error("Native input requires UUID");
        if (options.systemPrompt)
          fs.appendFileSync(
            options.cwd + "/.sdk-inputs",
            JSON.stringify(input.value) + "\n",
          );
        let text = input.value.message.content[0].text;
        yield { type: "user", isReplay: true, uuid: input.value.uuid };
        // SDK shouldQuery:false appends context for the next querying message;
        // its receipt must not generate or complete an assistant turn.
        if (input.value.shouldQuery === false) {
          transcriptPrefix.push(text);
          yield {
            type: "result",
            subtype: "success",
            num_turns: 0,
            user_message_uuid: input.value.uuid,
          };
          continue;
        }
        text = [...transcriptPrefix, text].join("\n\n");
        transcriptPrefix = [];
        turnAbort = new AbortController();
        const turnSignal = turnAbort.signal;
        yield { type: "system", subtype: "init" };
        if (text === "steer-race") {
          await raceStarted;
          yield {
            type: "result",
            subtype: "success",
            usage: { input_tokens: 10, output_tokens: 2 },
            result: "Before steer",
          };
          releaseRace();
          continue;
        }
        if (text === "steer") {
          yield {
            type: "stream_event",
            event: { type: "message_start", message: { id: "waiting" } },
          };
          yield {
            type: "stream_event",
            event: {
              type: "content_block_delta",
              delta: { type: "text_delta", text: "Waiting for steer" },
            },
          };
          const next = await pendingPrompt;
          pendingPrompt = null;
          pendingPrompt = prompt
            .next()
            .then((result) => ({ kind: "prompt", result }));
          if (next.kind !== "prompt") throw new Error("Expected a steer input");
          const nextInput = next.result;
          if (
            !/^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/i.test(
              nextInput.value.uuid,
            )
          )
            throw new Error("Native steer requires UUID");
          yield { type: "user", isReplay: true, uuid: nextInput.value.uuid };
          text = nextInput.value.message.content[0].text;
          if (text !== "replacement") throw new Error("Wrong steer");
          while (
            !fs.existsSync(options.cwd + "/.release-steer") &&
            !abort.signal.aborted
          )
            await new Promise((r) => setTimeout(r, 5));
        }
        if (text === "plan") {
          const decision = await options.canUseTool(
            "ExitPlanMode",
            { plan: "A visible plan" },
            { toolUseID: "plan", signal: turnSignal },
          );
          if (decision.behavior !== "deny")
            throw new Error("Plan executed without a separate user turn");
        }
        if (text === "permission") {
          const decision = await options.canUseTool(
            "Bash",
            { command: "echo hello" },
            { toolUseID: "tool-1", signal: turnSignal },
          );
          if (decision.behavior !== "deny") throw new Error("Expected denial");
        }
        if (text === "question") {
          const decision = await options.canUseTool(
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
            { toolUseID: "ask", signal: turnSignal },
          );
          if (decision.updatedInput.answers["Choose?"] !== "A")
            throw new Error("Missing answer");
        }
        if (text === "question-multi") {
          const decision = await options.canUseTool(
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
            { toolUseID: "ask-multi", signal: turnSignal },
          );
          if (decision.updatedInput.answers["Features?"] !== "A, B")
            throw new Error("Missing multi answer");
        }
        if (text === "tool") {
          const tool = options.mcpServers.studio.tools[0];
          const result = await tool.call(
            { text: "hello" },
            { signal: turnSignal },
          );
          if (result.content[0].text !== "tool-ok")
            throw new Error("Tool result lost");
        }
        if (text === "tool-pre-aborted") {
          const tool = options.mcpServers.studio.tools[0];
          const cancelled = new AbortController();
          cancelled.abort();
          await tool.call({ text: "hello" }, { signal: cancelled.signal });
        }
        if (text.startsWith("wait"))
          await new Promise((resolve) => {
            if (turnSignal.aborted) resolve();
            else turnSignal.addEventListener("abort", resolve, { once: true });
          });
        if (text === "fail") throw new Error("Native failure");
        if (text === "/compact")
          yield {
            type: "system",
            subtype: "local_command_output",
            uuid: "compact-result",
            content: "Native compact received",
          };
        if (
          text === "background" ||
          text === "background-hold" ||
          text === "background-stop-hold"
        ) {
          backgroundTasks.set("worker-1", true);
          yield {
            type: "system",
            subtype: "task_started",
            task_id: "worker-1",
            description: "Worker",
            task_type: "agent",
          };
        }
        if (text === "background-stop-hold") {
          fs.writeFileSync(options.cwd + "/.background-waiting", "yes");
          await new Promise((resolve) => {
            if (turnSignal.aborted) resolve();
            else turnSignal.addEventListener("abort", resolve, { once: true });
          });
          if (stoppedTasks.has("worker-1")) {
            backgroundTasks.delete("worker-1");
            yield {
              type: "system",
              subtype: "task_notification",
              task_id: "worker-1",
              status: "killed",
            };
          }
          yield {
            type: "result",
            subtype: "success",
            usage: { input_tokens: 1, output_tokens: 0 },
            result: "",
          };
          continue;
        }
        if (text === "claude-limit") {
          const resetsAt = Math.floor(Date.now() / 1000) + 3600;
          yield {
            type: "rate_limit_event",
            rate_limit_info: {
              status: "rejected",
              rateLimitType: "five_hour",
              resetsAt,
              utilization: 1,
            },
          };
          yield {
            type: "result",
            subtype: "error",
            errors: [
              "Claude usage limit reached (five_hour). Resets at " +
                new Date(resetsAt * 1000).toISOString() +
                ". The turn waits.",
            ],
          };
          continue;
        }
        if (text === "claude-offline") {
          const offline =
            "API Error: Can't reach the API server \u2014 check your internet or DNS (ENOTFOUND)";
          yield {
            type: "assistant",
            error: "unknown",
            message: {
              id: "offline",
              content: [{ type: "text", text: offline }],
            },
          };
          yield {
            type: "result",
            subtype: "success",
            is_error: true,
            result: offline,
          };
          continue;
        }
        if (text.startsWith("Continue the previous turn"))
          fs.writeFileSync(options.cwd + "/.continued", text);
        if (text === "subagent-text") {
          yield {
            type: "assistant",
            parent_tool_use_id: "agent-tool",
            uuid: "worker-message",
            message: {
              id: "worker-answer",
              content: [
                { type: "text", text: "First worker paragraph" },
                { type: "text", text: "Second worker paragraph" },
              ],
            },
          };
        }
        if (text === "snapshots") {
          yield {
            type: "system",
            subtype: "local_command_output",
            uuid: "command-output",
            content: "Native command output",
          };
          yield {
            type: "assistant",
            message: {
              id: "snapshot-only",
              content: [{ type: "text", text: "Snapshot without stream" }],
            },
          };
          yield {
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
          };
          yield {
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
          };
        }
        if (text === "split-blocks") {
          yield {
            type: "assistant",
            uuid: "11111111-1111-4111-8111-111111111111",
            message: {
              id: "split-message",
              content: [{ type: "text", text: "First text block." }],
              stop_reason: null,
            },
          };
          yield {
            type: "assistant",
            uuid: "22222222-2222-4222-8222-222222222222",
            message: {
              id: "split-message",
              content: [{ type: "text", text: "Second text block." }],
              stop_reason: null,
            },
          };
          yield {
            type: "assistant",
            uuid: "22222222-2222-4222-8222-222222222222",
            message: {
              id: "split-message",
              content: [{ type: "text", text: "Second text block." }],
              stop_reason: null,
            },
          };
          yield {
            type: "result",
            subtype: "success",
            usage: { input_tokens: 10, output_tokens: 2 },
            result: "First text block. Second text block.",
          };
          continue;
        }
        if (text === "rate-usage") {
          yield {
            type: "stream_event",
            event: { type: "message_start", message: { id: "rate-1" } },
          };
          yield {
            type: "stream_event",
            event: {
              type: "content_block_delta",
              delta: { type: "text_delta", text: "Rate answer" },
            },
          };
          for (const output of [6, 6, 8])
            yield {
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
            };
          yield {
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
          };
          outputTotal += 20;
          yield {
            type: "result",
            subtype: "success",
            usage: { input_tokens: 3000, output_tokens: outputTotal },
            result: "Second answer",
          };
          continue;
        }
        if (text.startsWith("split-stream")) {
          yield {
            type: "stream_event",
            event: { type: "message_start", message: { id: "stream-blocks" } },
          };
          yield {
            type: "stream_event",
            event: {
              type: "content_block_delta",
              index: 0,
              delta: { type: "text_delta", text: "First stream block." },
            },
          };
          yield {
            type: "assistant",
            uuid: "11111111-1111-4111-8111-111111111111",
            message: {
              id: "stream-blocks",
              content: [{ type: "text", text: "First stream block." }],
              stop_reason: null,
            },
          };
          yield {
            type: "stream_event",
            event: {
              type: "content_block_delta",
              index: 1,
              delta: { type: "text_delta", text: "Second stream block." },
            },
          };
          yield {
            type: "assistant",
            uuid: "22222222-2222-4222-8222-222222222222",
            message: {
              id: "stream-blocks",
              content: [{ type: "text", text: "Second stream block." }],
              stop_reason: null,
            },
          };
          yield {
            type: "result",
            subtype: "success",
            usage: { input_tokens: 10, output_tokens: 2 },
            result: "First stream block. Second stream block.",
          };
          continue;
        }
        yield {
          type: "stream_event",
          event: { type: "message_start", message: { id: "answer" } },
        };
        yield {
          type: "stream_event",
          event: {
            type: "content_block_delta",
            delta: { type: "text_delta", text: "Visible answer" },
          },
        };
        yield {
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
        };
        yield {
          type: "result",
          subtype: "success",
          usage: { input_tokens: 10, output_tokens: 2 },
          result: "Visible answer",
        };
        if (text === "background") {
          yield {
            type: "assistant",
            parent_tool_use_id: "agent-tool",
            uuid: "late-worker",
            message: {
              id: "late-answer",
              content: [{ type: "text", text: "Late worker text" }],
            },
          };
          yield {
            type: "system",
            subtype: "task_notification",
            task_id: "worker-1",
            status: "completed",
            summary: "Worker finished",
          };
          backgroundTasks.delete("worker-1");
        }
      }
    },
  };
}
