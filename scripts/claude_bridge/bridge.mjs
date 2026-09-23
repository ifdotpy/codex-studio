// Claude Agent SDK adapter for Studio's existing local session protocol.
import {
  query,
  createSdkMcpServer,
  tool,
  forkSession,
  getSessionMessages,
} from "@anthropic-ai/claude-agent-sdk";
import {
  InputQueue,
  usageLimits,
  limitEvent,
  nativeItem,
  usageTokens,
} from "./features.mjs";
import {
  commandCatalog,
  explicitNativeCommand,
  rollbackSession,
  forkAtTurn,
} from "./controls.mjs";
import { z } from "zod";
import { createHash, randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
import fs from "node:fs/promises";
import path from "node:path";
import { createCommandTransport, commandMethods } from "./commands.mjs";

import { thinkingFlag } from "./thinking.mjs";

const providerOptions = JSON.parse(process.env.STUDIO_CLAUDE_OPTIONS || "{}");
let lastLimits;
const root = process.argv[2];
if (!root) throw new Error("Claude bridge requires its own state directory");
await fs.mkdir(path.join(root, "sessions"), { recursive: true, mode: 0o700 });
const sessions = new Map(),
  queries = new Map(),
  pending = new Map();
const send = (value) => process.stdout.write(JSON.stringify(value) + "\n");
const emit = (method, params) => send({ method, params });
const commands = createCommandTransport({ root, emit });
const request = (method, params, signal) =>
  new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(new Error("Claude request cancelled"));
    const id = "claude:" + randomUUID();
    const abort = () => {
      pending.delete(id);
      reject(new Error("Claude request cancelled"));
    };
    const cleanup = () => signal?.removeEventListener("abort", abort);
    pending.set(id, {
      resolve: (value) => {
        cleanup();
        resolve(value);
      },
      reject: (error) => {
        cleanup();
        reject(error);
      },
    });
    signal?.addEventListener("abort", abort, { once: true });
    send({ id, method, params });
  });
const sessionPath = (id) => {
  if (!/^[a-f0-9-]{36}$/.test(id))
    throw new Error("Invalid Claude session identity");
  return path.join(root, "sessions", id + ".json");
};
const writes = new Map();
const persist = (session) => {
  const previous = writes.get(session.id) || Promise.resolve();
  const next = previous.catch(() => {}).then(() => save(session));
  writes.set(session.id, next);
  return next;
};
const save = async (session) => {
  const file = sessionPath(session.id),
    tmp = file + "." + randomUUID() + ".tmp";
  await fs.writeFile(tmp, JSON.stringify(session), { mode: 0o600 });
  await fs.rename(tmp, file);
};
async function session(id) {
  if (!sessions.has(id)) {
    const saved = JSON.parse(await fs.readFile(sessionPath(id), "utf8"));
    for (const turn of saved.turns)
      if (turn.status === "inProgress") {
        turn.status = "interrupted";
        turn.error = {
          message:
            "Claude connection ended. Review the saved transcript before continuing.",
        };
      }
    sessions.set(id, saved);
  }
  return sessions.get(id);
}
const settings = (s) => ({
  cwd: s.cwd,
  pathToClaudeCodeExecutable: process.env.STUDIO_CLAUDE_BIN || "claude",
  settingSources: ["user", "project", "local"],
  includePartialMessages: true,
  env: { ...process.env },
  stderr: (line) => process.stderr.write(line),
});
function checkAccount(account) {
  if (
    !process.env.STUDIO_CLAUDE_ACCOUNT ||
    account.email !== process.env.STUDIO_CLAUDE_ACCOUNT ||
    account.apiProvider !== "firstParty" ||
    !account.subscriptionType ||
    (account.apiKeySource && account.apiKeySource !== "none")
  ) {
    throw new Error(
      "Claude Code account or subscription changed. Restore the original login.",
    );
  }
}
async function probe(cwd, read) {
  let release;
  const hold = new Promise((r) => (release = r));
  async function* prompt() {
    await hold;
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 20000);
  const q = query({
    prompt: prompt(),
    options: {
      ...settings({ cwd: cwd || root }),
      persistSession: false,
      abortController: controller,
      settings: { disableAllHooks: true },
      mcpServers: {},
      strictMcpConfig: true,
      allowedTools: [],
      env: {
        ...process.env,
        ENABLE_CLAUDEAI_MCP_SERVERS: "false",
        CLAUDE_CODE_AUTO_CONNECT_IDE: "0",
      },
    },
  });
  try {
    checkAccount(await q.accountInfo());
    return await read(q);
  } finally {
    clearTimeout(timer);
    release();
    q.close();
  }
}
let metadata,
  metadataAt = 0;
async function catalog() {
  if (metadata && Date.now() - metadataAt < 300000) return metadata;
  metadataAt = Date.now();
  metadata = probe(null, async (q) => ({
    models: await q.supportedModels(),
    account: await q.accountInfo(),
  }));
  try {
    return await metadata;
  } catch (error) {
    metadata = null;
    throw error;
  }
}
function wireThread(s) {
  return {
    historyVersion: createHash("sha256")
      .update(JSON.stringify([s.nativeId || s.id, s.turns]))
      .digest("hex"),
    id: s.id,
    cwd: s.cwd,
    createdAt: s.createdAt,
    updatedAt: s.updatedAt || s.createdAt,
    preview: s.preview || "",
    turns: s.turns,
    status: { type: queries.get(s.id)?.turn ? "active" : "idle" },
    modelProvider: "claude",
  };
}
async function permissions(s, turn, name, input, options) {
  if (!turn)
    return {
      behavior: "deny",
      message: "No active Claude turn owns this request.",
    };
  if (name === "ExitPlanMode") {
    const plan = input.plan || input.planMarkdown || input.plan_markdown;
    if (typeof plan === "string" && plan)
      notice(s, turn, plan, options.toolUseID + ":plan");
    return {
      behavior: "deny",
      message:
        "The plan is shown in the chat. Stop and wait for a separate implementation request.",
    };
  }
  if (name.startsWith("mcp__studio__"))
    return { behavior: "allow", updatedInput: input };
  if (name === "AskUserQuestion") {
    const questions = (input.questions || []).map((q, i) => ({
      ...q,
      id: String(i),
      isOther: true,
      isSecret: false,
    }));
    const result = await request(
      "item/tool/requestUserInput",
      { threadId: s.id, turnId: turn.id, itemId: options.toolUseID, questions },
      options.signal,
    );
    const answers = Object.fromEntries(
      questions.map((q) => [
        q.question,
        (result.answers?.[q.id]?.answers || []).join(", "),
      ]),
    );
    return { behavior: "allow", updatedInput: { ...input, answers } };
  }
  const result = await request(
    "item/commandExecution/requestApproval",
    {
      threadId: s.id,
      turnId: turn.id,
      itemId: options.toolUseID,
      command:
        name === "Bash" ? input.command : JSON.stringify({ tool: name, input }),
      cwd: s.cwd,
      reason: "Claude requests permission to use " + name,
    },
    options.signal,
  );
  return ["accept", "acceptForSession"].includes(result.decision)
    ? { behavior: "allow", updatedInput: input }
    : { behavior: "deny", message: "The user declined this action." };
}
function studioTools(s, getTurn) {
  return createSdkMcpServer({
    name: "studio",
    version: "1.0.0",
    tools: (s.dynamicTools || []).map((definition) => {
      const schema = z.fromJSONSchema(definition.inputSchema);
      return tool(
        definition.name,
        definition.description,
        schema.shape,
        async (args) => {
          const result = await request("item/tool/call", {
            threadId: s.id,
            turnId: getTurn()?.id,
            callId: randomUUID(),
            tool: definition.name,
            arguments: args,
          });
          return {
            isError: result.success === false,
            content: (result.contentItems || []).map((item) => ({
              type: "text",
              text: item.text || JSON.stringify(item),
            })),
          };
        },
      );
    }),
  });
}
async function content(input) {
  const result = [];
  for (const item of input || []) {
    if (item.type === "text") result.push({ type: "text", text: item.text });
    else if (item.type === "localImage") {
      const bytes = await fs.readFile(item.path);
      const ext = path.extname(item.path).toLowerCase();
      const media_type = {
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".gif": "image/gif",
        ".webp": "image/webp",
      }[ext];
      if (!media_type) throw new Error("Unsupported image format for Claude");
      result.push({
        type: "image",
        source: { type: "base64", media_type, data: bytes.toString("base64") },
      });
    } else throw new Error("Unsupported Claude input: " + item.type);
  }
  return result;
}
function turnEvent(s, turn, method, item) {
  emit(method, { threadId: s.id, turnId: turn?.id, item });
}
function finishItem(s, turn, item) {
  if (!turn) return;
  const i = turn.items.findIndex((old) => old.id === item.id);
  if (i < 0) turn.items.push(item);
  else turn.items[i] = item;
  turnEvent(s, turn, "item/completed", item);
}
function notice(s, turn, text, id = randomUUID()) {
  finishItem(s, turn, { id, type: "agentMessage", phase: "commentary", text });
}
const permissionMode = (s, p = {}) =>
  s.claude?.permissionMode ||
  ((p.approvalPolicy || s.approvalPolicy) === "never"
    ? "bypassPermissions"
    : s.sandbox === "read-only"
      ? "plan"
      : "default");
async function flags(s, p = {}, live = false) {
  const native = await catalog();
  return {
    fastMode: p.serviceTier === "priority",
    ...(p.effort !== undefined ? { effortLevel: p.effort } : {}),
    ...thinkingFlag(p.model || s.model, native.models, s.claude?.thinking, live),
    ...(s.claude?.autoCompactWindow || providerOptions.autoCompactWindow
      ? {
          autoCompactWindow:
            s.claude?.autoCompactWindow || providerOptions.autoCompactWindow,
        }
      : {}),
  };
}
async function finishTurn(s, active, result, error) {
  const turn = active.turn;
  if (!turn) return;
  turn.status = turn.interrupted
    ? "interrupted"
    : error || result?.is_error || result?.subtype !== "success"
      ? "failed"
      : "completed";
  if (error || turn.status === "failed")
    turn.error = {
      message:
        error?.message ||
        result?.errors?.join("\n") ||
        result?.result ||
        "Claude turn failed",
    };
  const answer = turn.items
    .filter(
      (i) =>
        i.type === "agentMessage" &&
        !i.nativeSubagent &&
        !i.thinking &&
        !i.id.endsWith(":thinking"),
    )
    .at(-1);
  if (answer) finishItem(s, turn, { ...answer, phase: "final_answer" });
  const usage = usageTokens(result?.usage);
  if (usage) {
    // SDK result usage is cumulative over a persistent query. Keep the delta.
    const aggregate = Number.isFinite(result.usage.total_tokens)
      ? result.usage.total_tokens
      : usage.totalTokens;
    const delta = Math.max(0, aggregate - active.reportedUsage);
    active.reportedUsage = aggregate;
    s.totalTokens = (s.totalTokens || 0) + delta;
    const window =
      Math.max(
        0,
        ...Object.values(result?.modelUsage || {}).map(
          (m) => m.contextWindow || 0,
        ),
      ) || s.contextWindow;
    s.contextWindow = window;
    emit("thread/tokenUsage/updated", {
      threadId: s.id,
      turnId: turn.id,
      tokenUsage: {
        total: { totalTokens: s.totalTokens },
        last: active.lastUsage || usage,
        modelContextWindow: window,
      },
    });
  }
  s.updatedAt = Math.floor(Date.now() / 1000);
  await persist(s);
  active.turn = null;
  active.pendingSteers.clear();
  emit("turn/completed", {
    threadId: s.id,
    turn: { id: turn.id, status: turn.status, error: turn.error },
  });
}
async function startSession(s, active, p) {
  const initial = active.turn;
  let admit;
  const admitted = new Promise((r) => (admit = r));
  let allowed = false;
  async function* prompt() {
    await admitted;
    if (!allowed) return;
    yield* active.input;
  }
  try {
    const mode = permissionMode(s, p);
    const q = query({
      prompt: prompt(),
      options: {
        ...settings(s),
        model: p.model || s.model,
        ...(s.started
          ? { resume: s.nativeId || s.id }
          : { sessionId: s.nativeId || s.id }),
        systemPrompt: {
          type: "preset",
          preset: "claude_code",
          append: s.developerInstructions || "",
        },
        // Studio managed agents replace native subagents, as in Codex threads.
        disallowedTools: ["Agent"],
        permissionMode: mode,
        allowDangerouslySkipPermissions: true,
        ...(p.effort ? { effort: p.effort } : {}),
        settings: await flags(s, p),
        extraArgs: {
          ...providerOptions.extraArgs,
          "replay-user-messages": null,
        },
        canUseTool: (name, input, options) =>
          permissions(s, active.turn, name, input, options),
        mcpServers: { studio: studioTools(s, () => active.turn) },
        supportedDialogKinds: ["resume_return"],
        onUserDialog: async (value, options) => {
          if (value.dialogKind !== "resume_return" || !active.turn)
            return { behavior: "cancelled" };
          const question =
            "Resume this Claude conversation with a summary or full history?";
          const answer = await permissions(
            s,
            active.turn,
            "AskUserQuestion",
            {
              questions: [
                {
                  question,
                  header: "Resume",
                  options: [
                    {
                      label: "Compact and continue",
                      description: "Use a summary.",
                    },
                    {
                      label: "Keep full history",
                      description: "Keep the complete context.",
                    },
                    {
                      label: "Never ask again",
                      description: "Keep the history and disable this prompt.",
                    },
                  ],
                },
              ],
            },
            { ...options, toolUseID: value.toolUseID || options.requestId },
          );
          const selected = answer.updatedInput?.answers?.[question];
          return selected
            ? {
                behavior: "completed",
                result:
                  selected === "Compact and continue"
                    ? "compact"
                    : selected === "Never ask again"
                      ? "never"
                      : "continue",
              }
            : { behavior: "cancelled" };
        },
        onElicitation: (value, options) =>
          request(
            "mcpServer/elicitation/request",
            {
              threadId: s.id,
              turnId: active.turn?.id,
              serverName: value.serverName,
              request: value,
              ...value,
            },
            options.signal,
          ),
      },
    });
    active.q = q;
    checkAccount(await q.accountInfo());
    allowed = true;
    admit();
    active.readyResolve();
    if (initial?.interrupted) {
      q.close();
      throw new Error("Claude interrupted before start");
    }
    for await (const m of q) {
      if (m.type === "system" && m.subtype === "init") {
        s.started = true;
        s.claudeVersion = m.claude_code_version;
        active.commands = m.slash_commands || [];
        await persist(s);
        continue;
      }
      if (m.type === "system" && m.subtype === "background_tasks_changed") {
        active.tasks = new Map(m.tasks.map((t) => [t.task_id, t]));
        continue;
      }
      if (m.type === "user" && m.isReplay) {
        active.pendingSteers.delete(m.uuid);
        active.deferredResult = null;
        const item = active.turn?.items.find(
          (i) =>
            i.id === m.uuid ||
            (i.type === "userMessage" && active.turn.id === m.uuid),
        );
        if (item) item.nativeId = m.uuid;
        continue;
      }
      if (
        !active.turn &&
        ["assistant", "stream_event"].includes(m.type) &&
        !m.parent_tool_use_id
      ) {
        active.turn = {
          id: randomUUID(),
          status: "inProgress",
          items: [],
          synthetic: true,
        };
        s.turns.push(active.turn);
        emit("turn/started", {
          threadId: s.id,
          turn: { id: active.turn.id, status: "inProgress" },
        });
      }
      const turn =
        active.turn || (m.parent_tool_use_id ? s.turns.at(-1) : null);
      if (m.type === "rate_limit_event") {
        const next = limitEvent(lastLimits, m.rate_limit_info || {});
        if (next) {
          next.accountId = "claude:" + process.env.STUDIO_CLAUDE_ACCOUNT;
          lastLimits = next;
          emit("account/rateLimits/updated", next);
        }
        const info = m.rate_limit_info || {};
        if (
          turn &&
          info.status === "rejected" &&
          !info.isUsingOverage &&
          !info.overageInUse &&
          !["allowed", "allowed_warning"].includes(info.overageStatus)
        ) {
          notice(
            s,
            turn,
            "Claude usage limit reached (" +
              (info.rateLimitType || "unknown") +
              "). " +
              (info.resetsAt
                ? "Resets at " +
                  new Date(info.resetsAt * 1000).toISOString() +
                  ". "
                : "") +
              "The turn waits. You can pause it.",
            "limit:" + turn.id + ":" + info.rateLimitType + ":" + info.resetsAt,
          );
        }
        continue;
      }
      if (m.type === "system") {
        if (m.subtype === "compact_boundary" && turn) {
          const item = {
            id: m.uuid || randomUUID(),
            type: "contextCompaction",
            status: "completed",
            compactionMetadata: m.compact_metadata,
          };
          finishItem(s, turn, item);
          active.lastUsage = null;
          await persist(s);
        } else if (
          m.subtype === "status" &&
          m.status === "compacting" &&
          turn
        ) {
          notice(
            s,
            turn,
            "Claude is compacting the conversation.",
            "compacting:" + turn.id,
          );
        } else if (m.subtype === "local_command_output" && turn) {
          notice(
            s,
            turn,
            m.content || m.output || "Claude command completed.",
            m.uuid,
          );
        } else if (m.subtype?.startsWith("task_")) {
          const id = m.task_id || m.uuid;
          const task = { ...active.tasks.get(id), ...m };
          if (
            m.subtype === "task_notification" ||
            ["completed", "failed", "killed"].includes(m.status)
          )
            active.tasks.delete(id);
          else active.tasks.set(id, task);
          const taskTurn = turn || s.turns.at(-1);
          if (taskTurn)
            finishItem(s, taskTurn, {
              id: "task:" + id,
              type: "mcpToolCall",
              server: "claude",
              tool: m.task_type || "Background task",
              status:
                m.subtype === "task_notification"
                  ? m.status === "failed"
                    ? "failed"
                    : "completed"
                  : "inProgress",
              arguments: { description: m.description || task.description },
              result: {
                content: [
                  {
                    type: "text",
                    text:
                      m.summary ||
                      m.last_tool_name ||
                      m.description ||
                      m.status ||
                      "Running",
                  },
                ],
              },
            });
          await persist(s);
        } else if (
          turn &&
          ((m.subtype === "notification" &&
            ["high", "immediate"].includes(m.priority)) ||
            [
              "model_refusal_fallback",
              "model_refusal_no_fallback",
              "mirror_error",
            ].includes(m.subtype) ||
            (m.subtype === "informational" && m.level === "warning"))
        )
          notice(s, turn, m.text || m.content || m.error || m.subtype, m.uuid);
        else if (m.subtype === "api_retry" && turn)
          emit("item/agentMessage/delta", {
            threadId: s.id,
            turnId: turn.id,
            itemId: "retry:" + turn.id,
            delta: "",
          });
        continue;
      }
      if (!turn) continue;
      if (m.parent_tool_use_id) {
        if (m.type === "assistant")
          for (const [index, block] of (m.message.content || []).entries()) {
            const text =
              block.type === "text"
                ? block.text
                : block.type === "thinking"
                  ? block.thinking
                  : null;
            if (text)
              finishItem(s, turn, {
                id: (m.uuid || m.message.id) + ":" + block.type + ":" + index,
                type: "agentMessage",
                phase: "commentary",
                nativeSubagent: true,
                text: "Subagent: " + text,
              });
          }
        await persist(s);
        continue;
      }
      if (m.type === "tool_progress") {
        const item = active.tools.get(m.tool_use_id);
        if (item)
          turnEvent(s, turn, "item/started", {
            ...item,
            elapsedSeconds: m.elapsed_time_seconds,
          });
      } else if (m.type === "stream_event") {
        const e = m.event;
        if (e.type === "message_start") active.messageId = e.message.id;
        if (
          e.type === "content_block_delta" &&
          ["text_delta", "thinking_delta"].includes(e.delta.type)
        ) {
          const thinking = e.delta.type === "thinking_delta";
          const id = active.messageId + (thinking ? ":thinking" : "");
          const delta = e.delta.text || e.delta.thinking || "";
          let item = turn.items.find((i) => i.id === id);
          if (!item) {
            item = {
              id,
              type: "agentMessage",
              phase: "commentary",
              text: "",
              thinking,
            };
            turn.items.push(item);
          }
          item.text += delta;
          emit("item/agentMessage/delta", {
            threadId: s.id,
            turnId: turn.id,
            itemId: id,
            delta,
          });
        }
      } else if (m.type === "assistant") {
        active.lastUsage = usageTokens(m.message.usage) || active.lastUsage;
        const text = m.message.content
          .filter((b) => b.type === "text")
          .map((b) => b.text)
          .join("\n");
        if (text)
          finishItem(s, turn, {
            id: m.message.id,
            nativeId: m.uuid,
            type: "agentMessage",
            text,
            phase: "commentary",
          });
        const thinking = m.message.content
          .filter((b) => b.type === "thinking" && b.thinking)
          .map((b) => b.thinking)
          .join("\n");
        if (thinking)
          finishItem(s, turn, {
            id: m.message.id + ":thinking",
            nativeId: m.uuid,
            type: "agentMessage",
            text: thinking,
            phase: "commentary",
          });
        for (const b of m.message.content.filter(
          (b) => b.type === "tool_use",
        )) {
          const item = nativeItem(b, s.cwd);
          active.tools.set(b.id, item);
          turn.items.push(item);
          turnEvent(s, turn, "item/started", item);
          if (b.name === "TodoWrite")
            emit("turn/plan/updated", {
              threadId: s.id,
              turnId: turn.id,
              plan:
                b.input.todos?.map((t) => ({
                  step: t.content,
                  status: t.status,
                })) || [],
            });
        }
        await persist(s);
      } else if (m.type === "user" && Array.isArray(m.message?.content)) {
        for (const b of m.message.content.filter(
          (b) => b.type === "tool_result",
        )) {
          const old = active.tools.get(b.tool_use_id);
          if (!old) continue;
          const text =
            typeof b.content === "string"
              ? b.content
              : JSON.stringify(b.content);
          finishItem(s, turn, {
            ...old,
            status: b.is_error ? "failed" : "completed",
            result: { content: b.content },
            ...(old.type === "commandExecution"
              ? { aggregatedOutput: text, exitCode: b.is_error ? 1 : 0 }
              : {}),
          });
        }
        await persist(s);
      } else if (m.type === "result") {
        if (
          (active.pendingSteers.size || active.reservingInput) &&
          !m.is_error &&
          !turn.interrupted
        ) {
          active.deferredResult = m;
          continue;
        }
        await finishTurn(s, active, m);
      }
    }
    if (active.turn) throw new Error("Claude ended without a completed turn");
  } catch (error) {
    active.readyReject(error);
    if (active.turn) await finishTurn(s, active, null, error);
  } finally {
    allowed = false;
    admit();
    active.input.close();
    active.q?.close();
    if (queries.get(s.id) === active) queries.delete(s.id);
  }
}
function newActive(turn) {
  const active = {
    turn,
    input: new InputQueue(),
    q: null,
    tasks: new Map(),
    tools: new Map(),
    pendingSteers: new Set(),
    reportedUsage: 0,
    lastUsage: null,
  };
  active.ready = new Promise((resolve, reject) => {
    active.readyResolve = resolve;
    active.readyReject = reject;
  });
  active.ready.catch(() => {});
  return active;
}
function userMessage(s, turn, blocks, id) {
  return {
    type: "user",
    uuid: id || turn.id,
    session_id: s.nativeId || s.id,
    parent_tool_use_id: null,
    message: { role: "user", content: blocks },
  };
}
async function handle(method, p) {
  if (commandMethods.has(method)) return commands.handle(method, p);
  if (
    p.threadId &&
    [
      "turn/start",
      "turn/steer",
      "thread/resume",
      "thread/fork",
      "claude/settings",
      "thread/compact/start",
    ].includes(method)
  ) {
    const current = await session(p.threadId);
    if (
      Object.values(current.controlRequests || {}).some(
        (r) => r.status !== "completed",
      )
    )
      throw new Error(
        "A Claude history operation needs recovery before continuing",
      );
  }
  if (method === "initialize")
    return {
      userAgent: "studio-claude-bridge",
      platform: process.platform,
      capabilities: { claudeVersion: 4 },
    };
  if (method === "initialized") return {};
  if (method === "model/list") {
    const native = await catalog();
    const models = [
      ...native.models,
      ...(providerOptions.customModels || [])
        .filter((m) => !native.models.some((n) => n.value === m.id))
        .map((m) => ({
          value: m.id,
          displayName: m.label,
          description: "Custom Claude model",
          supportsEffort: true,
          supportedEffortLevels: ["low", "medium", "high", "max"],
        })),
    ];
    return {
      data: models.map((model) => ({
        id: model.value,
        model: model.value,
        displayName: "Claude · " + model.displayName,
        description: model.description,
        isDefault: model.value === "default",
        hidden: false,
        supportedReasoningEfforts: (model.supportedEffortLevels || []).map(
          (reasoningEffort) => ({
            reasoningEffort,
            description: reasoningEffort,
          }),
        ),
        defaultReasoningEffort: model.supportsEffort ? "medium" : null,
        serviceTiers: model.supportsFastMode
          ? [{ id: "priority", name: "Fast" }]
          : [],
      })),
      nextCursor: null,
    };
  }
  if (method === "account/read") {
    const { account } = await catalog();
    return {
      account: {
        type: "claude",
        email: account.email,
        planType: account.subscriptionType,
      },
      requiresOpenaiAuth: false,
    };
  }
  if (method === "account/rateLimits/read") {
    const data = await probe(p.cwd, async (q) =>
      usageLimits(
        await q.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET({
          skipBehaviors: true,
        }),
      ),
    );
    data.accountId = "claude:" + process.env.STUDIO_CLAUDE_ACCOUNT;
    lastLimits = data;
    return data;
  }
  if (method === "claude/commands")
    return probe(p.cwd, async (q) =>
      commandCatalog((await q.initializationResult()).commands),
    );
  if (method === "claude/state") {
    const s = await session(p.threadId),
      active = queries.get(s.id);
    return {
      settings: s.claude || {},
      turns: s.turns.map((t) => ({
        id: t.id,
        status: t.status,
        text:
          t.items
            .find((i) => i.type === "userMessage")
            ?.content?.find((b) => b.type === "text")
            ?.text?.slice(0, 160) || "Claude turn",
      })),
      tasks: [...(active?.tasks.values() || [])],
      version: s.claudeVersion,
      nativeId: s.nativeId || s.id,
      contextWindow: s.contextWindow,
      totalTokens: s.totalTokens,
    };
  }
  if (method === "claude/settings") {
    const s = await session(p.threadId),
      active = queries.get(s.id);
    if (active?.turn || active?.tasks.size)
      throw new Error("Pause Claude before changing these settings");
    active?.input.close();
    active?.q?.close();
    queries.delete(s.id);
    s.claude = p.settings;
    await persist(s);
    return { settings: s.claude };
  }
  if (method === "thread/rollback") {
    const s = await session(p.threadId),
      active = queries.get(s.id);
    if (active?.turn || active?.tasks.size)
      throw new Error("Pause Claude before rollback");
    active?.input.close();
    active?.q?.close();
    queries.delete(s.id);
    return rollbackSession(s, p, { persist, getSessionMessages, forkSession });
  }
  if (method === "thread/start") {
    const s = {
      ...p,
      id: randomUUID(),
      createdAt: Math.floor(Date.now() / 1000),
      turns: [],
      started: false,
      model: p.model || "default",
    };
    sessions.set(s.id, s);
    await persist(s);
    return { ...p, thread: wireThread(s), model: s.model, sandbox: null };
  }
  if (method === "thread/turns/list" || method === "thread/turns/items/list") {
    const s = await session(p.threadId);
    let values;
    if (method === "thread/turns/list")
      values =
        p.sortDirection === "asc" ? [...s.turns] : [...s.turns].reverse();
    else {
      const t = s.turns.find((t) => t.id === p.turnId);
      if (!t) throw new Error("Unknown Claude turn");
      values = t.items;
    }
    const offset = p.cursor ? Number(p.cursor) : 0,
      limit = Math.max(1, Math.min(100, p.limit || 20));
    if (!Number.isSafeInteger(offset) || offset < 0)
      throw new Error("Invalid Claude history cursor");
    return {
      data: values.slice(offset, offset + limit),
      nextCursor:
        offset + limit < values.length ? String(offset + limit) : null,
    };
  }
  if (method === "thread/list") {
    const data = [];
    for (const file of await fs.readdir(path.join(root, "sessions")))
      if (file.endsWith(".json"))
        data.push(wireThread(await session(file.slice(0, -5))));
    return { data, nextCursor: null };
  }
  if (method === "skills/extraRoots/set")
    throw new Error("Claude uses its native skills and MCP configuration");
  if (method === "thread/resume" || method === "thread/read") {
    const s = await session(p.threadId);
    if (method === "thread/resume") {
      const active = queries.get(s.id);
      if (active?.turn || active?.tasks.size)
        throw new Error("Claude is still working");
      active?.input.close();
      active?.q?.close();
      queries.delete(s.id);
      Object.assign(s, p);
      await persist(s);
    }
    return {
      ...s,
      sandbox: null,
      thread: {
        ...wireThread(s),
        ...(!p.includeTurns && method === "thread/read" ? { turns: [] } : {}),
      },
    };
  }
  if (method === "thread/unsubscribe") {
    const active = queries.get(p.threadId);
    if (active?.turn || active?.tasks.size)
      throw new Error("Pause the Claude turn before detaching");
    active?.input.close();
    active?.q?.close();
    queries.delete(p.threadId);
    return {};
  }
  if (method === "thread/fork") {
    const old = await session(p.threadId);
    if (queries.get(old.id)?.turn || queries.get(old.id)?.tasks.size)
      throw new Error("Pause Claude before branching");
    const s = await forkAtTurn(old, p, {
      forkSession,
      getSessionMessages,
      persist,
    });
    sessions.set(s.id, s);
    return { ...s, sandbox: null, thread: wireThread(s) };
  }
  if (method === "thread/compact/start") {
    const s = await session(p.threadId);
    return handle("turn/start", {
      ...p,
      model: s.model,
      clientUserMessageId: p.requestId || randomUUID(),
      input: [{ type: "text", text: "/compact" }],
    });
  }
  if (method === "turn/start") {
    const s = await session(p.threadId);
    const prior = s.turns.find(
      (turn) =>
        turn.clientUserMessageId &&
        turn.clientUserMessageId === p.clientUserMessageId,
    );
    if (prior) {
      if (JSON.stringify(prior.items[0].content) !== JSON.stringify(p.input))
        throw new Error("This message identity has different content");
      return { turn: { id: prior.id, status: prior.status } };
    }
    if (queries.get(s.id)?.turn)
      throw new Error("Claude already has an active turn");
    let nativeCommand;
    if (p.claudeCommand) {
      const commands = await handle("claude/commands", { cwd: s.cwd });
      nativeCommand = explicitNativeCommand(p.claudeCommand, commands);
      // Unknown slash text remains ordinary user input; never extract commands from it.
    }
    const blocks = await content(
      nativeCommand ? [{ type: "text", text: nativeCommand }] : p.input,
    );
    const turn = {
      id: randomUUID(),
      clientUserMessageId: p.clientUserMessageId,
      status: "inProgress",
      items: [
        {
          id: p.clientUserMessageId || randomUUID(),
          type: "userMessage",
          content: p.input,
        },
      ],
    };
    let active = queries.get(s.id);
    if (active) {
      await active.ready;
      await active.q.setModel(p.model || s.model);
      await active.q.setPermissionMode(permissionMode(s, p));
      await active.q.applyFlagSettings(await flags(s, p, true));
      if (queries.get(s.id) !== active || active.input.closed)
        throw new Error("Claude closed before this turn could start");
      if (active.turn)
        throw new Error(
          "Claude background work started a turn; wait or steer that turn",
        );
      active.turn = turn;
      active.reservingInput = true;
    }
    s.model = p.model || s.model;
    s.turns.push(turn);
    s.preview =
      p.input?.find((item) => item.type === "text")?.text?.slice(0, 160) || "";
    try {
      await persist(s);
    } catch (error) {
      if (active?.turn === turn) {
        active.turn = null;
        active.reservingInput = false;
      }
      s.turns = s.turns.filter((t) => t !== turn);
      throw error;
    }
    if (active) {
      active.reservingInput = false;
      if (active.turn !== turn || active.input.closed)
        throw new Error("Claude stopped before accepting the new turn");
      active.lastUsage = null;
    } else {
      active = newActive(turn);
      queries.set(s.id, active);
      setImmediate(() => void startSession(s, active, p));
    }
    emit("turn/started", {
      threadId: s.id,
      turn: { id: turn.id, status: "inProgress" },
    });
    active.input.push(userMessage(s, turn, blocks, turn.id));
    return { turn: { id: turn.id, status: turn.status } };
  }
  if (method === "turn/steer") {
    const s = await session(p.threadId),
      active = queries.get(s.id);
    if (!active?.turn || active.turn.id !== p.expectedTurnId)
      throw new Error("The steer belongs to a different Claude turn");
    const turn = active.turn,
      id = p.clientUserMessageId;
    if (!id) throw new Error("Steer requires its original message identity");
    const previous = turn.items.find((item) => item.id === id);
    if (previous) {
      if (previous.deliveryStatus === "not_applied")
        throw new Error(previous.error);
      if (JSON.stringify(previous.content) !== JSON.stringify(p.input))
        throw new Error("This message identity has different content");
      return { turnId: turn.id };
    }
    active.pendingSteers.add(id);
    let item;
    try {
      const blocks = await content(p.input);
      if (
        active.turn !== turn ||
        queries.get(s.id) !== active ||
        active.input.closed
      )
        throw new Error("Claude finished before this steer could be submitted");
      item = {
        id,
        type: "userMessage",
        content: p.input,
        delivery: "steer",
        deliveryStatus: "preparing",
      };
      turn.items.push(item);
      await persist(s);
      if (
        active.turn !== turn ||
        queries.get(s.id) !== active ||
        active.input.closed
      )
        throw new Error("Claude stopped before this steer could be submitted");
      active.input.push(userMessage(s, turn, blocks, id));
      item.deliveryStatus = "submitted";
      return { turnId: turn.id };
    } catch (error) {
      active.pendingSteers.delete(id);
      if (item) {
        item.deliveryStatus = "not_applied";
        item.error = error.message;
        await persist(s);
      }
      if (
        active.deferredResult &&
        active.turn === turn &&
        !active.pendingSteers.size
      ) {
        const result = active.deferredResult;
        active.deferredResult = null;
        await finishTurn(s, active, result);
      }
      throw error;
    }
  }
  if (method === "turn/interrupt") {
    const active = queries.get(p.threadId);
    if (!active) return {};
    if (p.turnId && active.turn && p.turnId !== active.turn.id)
      throw new Error("The interrupt belongs to a different Claude turn");
    if (active.turn) active.turn.interrupted = true;
    active.input.close();
    active.q?.close();
    return {};
  }
  if (method === "thread/backgroundTerminals/list") {
    const data = [...(queries.get(p.threadId)?.tasks.values() || [])];
    return { data, terminals: data, nextCursor: null };
  }
  if (method === "thread/queue/list") {
    const data = [...(queries.get(p.threadId)?.pendingSteers || [])].map(
      (id) => ({ id, status: "queued" }),
    );
    return { data, items: data, nextCursor: null };
  }
  if (method === "claude/stopTask") {
    const active = queries.get(p.threadId);
    if (!active?.q || !active.tasks.has(p.taskId))
      throw new Error("This Claude background task is no longer active");
    await active.q.stopTask(p.taskId);
    return {};
  }
  throw new Error("Claude does not support " + method);
}
const operations = new Map();
const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (!message.method) {
    const waiting = pending.get(message.id);
    if (waiting) {
      pending.delete(message.id);
      message.error
        ? waiting.reject(new Error(message.error.message))
        : waiting.resolve(message.result);
    }
    return;
  }
  const params = message.params || {};
  // Serialize session mutations, but keep interrupt and permission replies independent.
  const key = params.threadId;
  const ordered =
    key &&
    [
      "turn/start",
      "turn/steer",
      "thread/resume",
      "thread/fork",
      "thread/read",
      "thread/rollback",
      "claude/settings",
      "claude/state",
      "thread/compact/start",
    ].includes(message.method);
  const before = ordered
    ? operations.get(key) || Promise.resolve()
    : Promise.resolve();
  const operation = before
    .catch(() => {})
    .then(() => handle(message.method, params));
  if (ordered) {
    operations.set(key, operation);
    operation
      .finally(() => {
        if (operations.get(key) === operation) operations.delete(key);
      })
      .catch(() => {});
  }
  operation.then(
    (result) => {
      if (message.id !== undefined) send({ id: message.id, result });
    },
    (error) => {
      if (message.id !== undefined)
        send({
          id: message.id,
          error: {
            code: Number.isInteger(error.code) ? error.code : -32000,
            message: error.message,
            ...(error.data === undefined ? {} : { data: error.data }),
          },
        });
    },
  );
});
let closing;
function shutdown() {
  if (closing) return closing;
  closing = (async () => {
    for (const { q, input } of queries.values()) {
      try {
        input.close();
        q?.close();
      } catch (error) {
        process.stderr.write("Claude shutdown: " + error.message + "\n");
      }
    }
    await commands.close();
    await Promise.allSettled(writes.values());
    process.exit(0);
  })();
  return closing;
}
lines.on("close", shutdown);
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
