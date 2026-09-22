// Claude Agent SDK adapter for Studio's existing local session protocol.
import {
  query,
  createSdkMcpServer,
  tool,
  forkSession,
} from "@anthropic-ai/claude-agent-sdk";
import { z } from "zod";
import { randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
import fs from "node:fs/promises";
import path from "node:path";

const root = process.argv[2];
if (!root) throw new Error("Claude bridge requires its own state directory");
await fs.mkdir(path.join(root, "sessions"), { recursive: true, mode: 0o700 });
const sessions = new Map(),
  queries = new Map(),
  pending = new Map();
const send = (value) => process.stdout.write(JSON.stringify(value) + "\n");
const emit = (method, params) => send({ method, params });
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
  env: { ...process.env, CLAUDE_CODE_DISABLE_BACKGROUND_TASKS: "1" },
  disallowedTools: ["Agent"],
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
let metadata;
async function catalog() {
  if (metadata) return metadata;
  metadata = (async () => {
    let finish;
    const hold = new Promise((resolve) => (finish = resolve));
    async function* prompt() {
      await hold;
    }
    const q = query({ prompt: prompt(), options: settings({ cwd: root }) });
    try {
      const account = await q.accountInfo();
      checkAccount(account);
      return { models: await q.supportedModels(), account };
    } finally {
      finish();
      q.close();
    }
  })();
  try {
    return await metadata;
  } catch (error) {
    metadata = null;
    throw error;
  }
}
function wireThread(s) {
  return {
    id: s.id,
    cwd: s.cwd,
    createdAt: s.createdAt,
    updatedAt: s.updatedAt || s.createdAt,
    preview: s.preview || "",
    turns: s.turns,
    status: { type: queries.has(s.id) ? "active" : "idle" },
    modelProvider: "claude",
  };
}
async function permissions(s, turn, name, input, options) {
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
function studioTools(s, turn) {
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
            turnId: turn.id,
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
async function runTurn(s, turn, params, blocks) {
  let admit;
  const admitted = new Promise((resolve) => (admit = resolve));
  let release;
  const hold = new Promise((resolve) => (release = resolve));
  async function* prompt() {
    await admitted;
    if (turn.interrupted) return;
    yield {
      type: "user",
      session_id: s.id,
      parent_tool_use_id: null,
      message: { role: "user", content: blocks },
    };
    await hold;
  }
  const tools = new Map();
  let activeMessage = null;
  try {
    if (turn.interrupted)
      throw new Error("Claude turn interrupted before start");
    const q = query({
      prompt: prompt(),
      options: {
        ...settings(s),
        model: params.model || s.model,
        ...(s.started ? { resume: s.id } : { sessionId: s.id }),
        systemPrompt: {
          type: "preset",
          preset: "claude_code",
          append: s.developerInstructions || "",
        },
        permissionMode:
          (params.approvalPolicy || s.approvalPolicy) === "never"
            ? "bypassPermissions"
            : s.sandbox === "read-only"
              ? "plan"
              : "default",
        ...((params.approvalPolicy || s.approvalPolicy) === "never"
          ? { allowDangerouslySkipPermissions: true }
          : {}),
        ...(params.effort || s.config?.model_reasoning_effort
          ? { effort: params.effort || s.config?.model_reasoning_effort }
          : {}),
        canUseTool: (name, input, options) =>
          permissions(s, turn, name, input, options),
        mcpServers: { studio: studioTools(s, turn) },
      },
    });
    queries.set(s.id, { q, turn, release });
    checkAccount(await q.accountInfo());
    admit();
    emit("turn/started", {
      threadId: s.id,
      turn: { id: turn.id, status: "inProgress" },
    });
    const event = (method, item) =>
      emit(method, { threadId: s.id, turnId: turn.id, item });
    const finishMessage = (item) => {
      const index = turn.items.findIndex((previous) => previous.id === item.id);
      if (index >= 0) turn.items[index] = item;
      else turn.items.push(item);
      event("item/completed", item);
    };
    for await (const message of q) {
      if (message.type === "system" && message.subtype === "init") {
        s.started = true;
        await persist(s);
      }
      if (message.type === "stream_event") {
        const e = message.event;
        if (e.type === "message_start")
          activeMessage = {
            id: e.message.id,
            type: "agentMessage",
            text: "",
            phase: "commentary",
          };
        if (
          e.type === "content_block_delta" &&
          e.delta.type === "text_delta" &&
          activeMessage
        ) {
          activeMessage.text += e.delta.text;
          emit("item/agentMessage/delta", {
            threadId: s.id,
            turnId: turn.id,
            itemId: activeMessage.id,
            delta: e.delta.text,
          });
        }
      } else if (message.type === "assistant") {
        const id = message.message.id;
        const text = message.message.content
          .filter((block) => block.type === "text")
          .map((block) => block.text)
          .join("\n");
        if (text)
          finishMessage({
            id,
            type: "agentMessage",
            text,
            phase: "commentary",
          });
        for (const block of message.message.content.filter(
          (block) => block.type === "tool_use",
        )) {
          const item = {
            id: block.id,
            type: "mcpToolCall",
            server: "claude",
            tool: block.name,
            arguments: block.input,
            status: "inProgress",
          };
          tools.set(block.id, item);
          turn.items.push(item);
          event("item/started", item);
        }
        await persist(s);
      } else if (
        message.type === "user" &&
        Array.isArray(message.message?.content)
      ) {
        for (const block of message.message.content.filter(
          (block) => block.type === "tool_result",
        )) {
          const previous = tools.get(block.tool_use_id);
          if (previous)
            finishMessage({
              ...previous,
              status: block.is_error ? "failed" : "completed",
              result: block.content,
            });
        }
        await persist(s);
      } else if (message.type === "result") {
        s.started = true;
        turn.status = turn.interrupted
          ? "interrupted"
          : message.is_error || message.subtype !== "success"
            ? "failed"
            : "completed";
        if (turn.status === "failed")
          turn.error = {
            message:
              message.errors?.join("\n") ||
              message.result ||
              "Claude turn failed",
          };
        const answer = turn.items
          .filter((item) => item.type === "agentMessage")
          .at(-1);
        if (answer) finishMessage({ ...answer, phase: "final_answer" });
        const usage = message.usage;
        if (usage) {
          const last = {
            inputTokens: usage.input_tokens || 0,
            outputTokens: usage.output_tokens || 0,
            cachedInputTokens: usage.cache_read_input_tokens || 0,
          };
          last.totalTokens =
            last.inputTokens +
            last.outputTokens +
            last.cachedInputTokens +
            (usage.cache_creation_input_tokens || 0);
          s.totalTokens = (s.totalTokens || 0) + last.totalTokens;
          emit("thread/tokenUsage/updated", {
            threadId: s.id,
            turnId: turn.id,
            tokenUsage: { total: { totalTokens: s.totalTokens }, last },
          });
        }
        s.updatedAt = Math.floor(Date.now() / 1000);
        await persist(s);
        break;
      }
    }
    if (turn.status === "inProgress")
      throw new Error("Claude ended without a completed turn");
  } catch (error) {
    turn.status = turn.interrupted ? "interrupted" : "failed";
    turn.error = { message: error.message };
    await persist(s);
  } finally {
    const active = queries.get(s.id);
    turn.interrupted ||= turn.status === "failed";
    admit();
    release();
    active?.q?.close();
    queries.delete(s.id);
    emit("turn/completed", {
      threadId: s.id,
      turn: { id: turn.id, status: turn.status, error: turn.error },
    });
  }
}
async function handle(method, p) {
  if (method === "initialize")
    return { userAgent: "studio-claude-bridge", platform: process.platform };
  if (method === "initialized") return {};
  if (method === "model/list") {
    const { models } = await catalog();
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
        serviceTiers: [],
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
  if (method === "account/rateLimits/read")
    throw new Error(
      "Claude Code does not expose account limits through this connection",
    );
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
    if (active) throw new Error("Pause the Claude turn before detaching");
    return {};
  }
  if (method === "thread/fork") {
    const old = await session(p.threadId);
    if (queries.has(old.id)) throw new Error("Pause Claude before branching");
    const fork = await forkSession(old.id, { dir: old.cwd });
    const s = {
      ...old,
      ...p,
      id: fork.sessionId,
      turns: structuredClone(old.turns),
    };
    sessions.set(s.id, s);
    await persist(s);
    return { ...s, sandbox: null, thread: wireThread(s) };
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
    if (queries.has(s.id)) throw new Error("Claude already has an active turn");
    const blocks = await content(p.input);
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
    s.turns.push(turn);
    s.preview =
      p.input?.find((item) => item.type === "text")?.text?.slice(0, 160) || "";
    await persist(s);
    queries.set(s.id, { turn, q: null });
    setImmediate(() => void runTurn(s, turn, p, blocks));
    return { turn: { id: turn.id, status: turn.status } };
  }
  if (method === "turn/interrupt") {
    const active = queries.get(p.threadId);
    if (active) {
      if (p.turnId && p.turnId !== active.turn.id)
        throw new Error("The interrupt belongs to a different Claude turn");
      active.turn.interrupted = true;
      await active.q?.interrupt();
    }
    return {};
  }
  if (method === "turn/steer")
    throw new Error(
      "Claude cannot steer this turn. Send the message to the queue.",
    );
  if (method === "thread/backgroundTerminals/list") return { terminals: [] };
  if (method === "thread/queue/list") return { items: [] };
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
    ["turn/start", "thread/resume", "thread/fork", "thread/read"].includes(
      message.method,
    );
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
          error: { code: -32000, message: error.message },
        });
    },
  );
});
lines.on("close", () => {
  for (const { q, release } of queries.values()) {
    release?.();
    q?.close();
  }
  process.exit(0);
});
