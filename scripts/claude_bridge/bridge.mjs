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
import { createSessionStore } from "./session-store.mjs";
import { claudeImage } from "./images.mjs";
import { listSkills } from "./skills.mjs";
import { reconcileHistoricalBash } from "./historical-bash-receipts.mjs";
import { createCatalogCache } from "./catalog-cache.mjs";

import {
  movePrompt,
  moveIdentity,
  moveVersions,
  nativeFile,
  nativeDestination,
  publishNative,
} from "./move.mjs";

const providerOptions = JSON.parse(process.env.STUDIO_CLAUDE_OPTIONS || "{}");
const STUDIO_INPUT_NAMESPACE = "8d95e191-763a-4ee2-a462-7d27f981f138";
function nativeUserMessageId(id) {
  if (
    /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i.test(
      id,
    )
  )
    return id;
  const namespace = Buffer.from(
    STUDIO_INPUT_NAMESPACE.replaceAll("-", ""),
    "hex",
  );
  const bytes = createHash("sha1")
    .update(namespace)
    .update(String(id))
    .digest()
    .subarray(0, 16);
  bytes[6] = (bytes[6] & 0x0f) | 0x50;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = bytes.toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
function assistantBlockState(active, messageId, kind) {
  const key = messageId + ":" + kind;
  if (!active.assistantBlocks.has(key))
    active.assistantBlocks.set(key, {
      blocks: [],
      streams: new Map(),
      frames: new Set(),
    });
  return active.assistantBlocks.get(key);
}
function assistantText(state) {
  return state.blocks
    .map((block) => block.text)
    .filter(Boolean)
    .join("\n");
}
let lastLimits;
const root = process.argv[2];
if (!root) throw new Error("Claude bridge requires its own state directory");
await fs.mkdir(path.join(root, "sessions"), { recursive: true, mode: 0o700 });
const queries = new Map(),
  pending = new Map();
const pendingTurnReceipts = new Set();
const PREPARATION_TIMEOUT_MS = 20_000;
// A cold Claude process can need more than 20 seconds under launchd limits.
// Retained query controls keep their shorter, shared deadline.
const INITIALIZATION_TIMEOUT_MS = 60_000;

async function boundedPreparation(read, deadline, onTimeout, phase, startedAt) {
  let timer;
  try {
    return await Promise.race([
      Promise.resolve().then(read),
      new Promise((_, reject) => {
        timer = setTimeout(
          () => {
            const error = Object.assign(
              new Error(
                "Claude preparation timed out before input was submitted",
              ),
              {
                preparationTimedOut: true,
                data: {
                  turnStartOutcome: "not_applied",
                  claudePreparationPhase: phase,
                  claudePreparationElapsedMs: Math.max(
                    0,
                    Date.now() - startedAt,
                  ),
                },
              },
            );
            reject(error);
            onTimeout();
          },
          Math.max(0, deadline - Date.now()),
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
const sessionStore = createSessionStore(root, {
  isPinned: (id) => queries.has(id),
});
const sessions = sessionStore.sessions;
const configuredIdle = Number(process.env.STUDIO_CLAUDE_IDLE_SECONDS);
const idleSeconds =
  Number.isFinite(configuredIdle) && configuredIdle > 0
    ? Math.max(1, configuredIdle)
    : 15 * 60;
const querySweep = setInterval(
  () => {
    const now = Date.now();
    for (const [id, active] of queries) {
      if (
        active.turn ||
        active.tasks.size ||
        active.pendingSteers.size ||
        active.reservingInput ||
        active.idleSince == null ||
        operations.get(id) ||
        now - active.idleSince < idleSeconds * 1000
      )
        continue;
      active.input.close();
      active.q?.close();
      if (queries.get(id) === active) {
        queries.delete(id);
        catalogCache.activityChanged();
        void sessionStore.evict(id);
      }
    }
  },
  Math.min(30000, idleSeconds * 500),
);
querySweep.unref();
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
const persist = sessionStore.persist;
async function session(id) {
  return sessionStore.get(id);
}
const settings = (s) => ({
  cwd: s.cwd,
  pathToClaudeCodeExecutable: process.env.STUDIO_CLAUDE_BIN || "claude",
  settingSources: ["user", "project", "local"],
  includePartialMessages: true,
  env: {
    ...process.env,
    ...(typeof s.studioImageWorkspaceTempDir === "string"
      ? { TMPDIR: s.studioImageWorkspaceTempDir }
      : {}),
  },
  stderr: (line) => process.stderr.write(line),
});
function checkAccount(account) {
  const reject = (reason, message) => {
    throw Object.assign(new Error(message), {
      claudeAccountValidationFailed: true,
      data: {
        turnStartOutcome: "not_applied",
        claudePreparationFailure: "account_validation",
        claudeAccountFailure: reason,
      },
    });
  };
  const expected = process.env.STUDIO_CLAUDE_ACCOUNT;
  if (!expected)
    reject(
      "expected_identity",
      "Claude Code account identity is not configured. No input was submitted.",
    );
  if (!account || typeof account !== "object" || Array.isArray(account))
    reject(
      "account_metadata",
      "Claude Code account metadata is missing. No input was submitted.",
    );
  if (typeof account.email !== "string" || !account.email.trim())
    reject(
      "email_metadata",
      "Claude Code email metadata is missing. No input was submitted.",
    );
  if (account.email !== expected)
    reject(
      "identity_mismatch",
      "Claude Code account or subscription changed: expected " +
        expected +
        ", got " +
        account.email +
        ". Restore the original login.",
    );
  if (typeof account.apiProvider !== "string" || !account.apiProvider)
    reject(
      "provider_metadata",
      "Claude Code API provider metadata is missing. No input was submitted.",
    );
  if (account.apiProvider !== "firstParty")
    reject(
      "provider_mismatch",
      "Claude Code API provider is " +
        account.apiProvider +
        ", expected firstParty. Restore the original login.",
    );
  if (
    typeof account.subscriptionType !== "string" ||
    !account.subscriptionType.trim()
  )
    reject(
      "subscription_metadata",
      "Claude Code subscription metadata is missing. No input was submitted.",
    );
  if (account.apiKeySource && account.apiKeySource !== "none")
    reject(
      "api_key_source",
      "Claude Code uses an API key instead of the configured subscription. Restore the original login.",
    );
}
async function verifiedAccount(
  q,
  deadline,
  onTimeout,
  startedAt,
  phase = "account",
) {
  const account = await boundedPreparation(
    () => q.accountInfo(),
    deadline,
    onTimeout,
    phase + "_initialize",
    startedAt,
  );
  try {
    checkAccount(account);
    return account;
  } catch (error) {
    if (
      ![
        "account_metadata",
        "email_metadata",
        "provider_metadata",
        "subscription_metadata",
      ].includes(error?.data?.claudeAccountFailure) ||
      typeof q.reinitialize !== "function" ||
      (account?.apiProvider && account.apiProvider !== "firstParty") ||
      (account?.apiKeySource && account.apiKeySource !== "none")
    )
      throw error;
    let fresh;
    try {
      // accountInfo keeps the first SDK initialization snapshot. Read once
      // from this same unsubmitted query, without extending its deadline.
      fresh = await boundedPreparation(
        () => q.reinitialize(),
        deadline,
        onTimeout,
        phase + "_reinitialize",
        startedAt,
      );
    } catch (refreshError) {
      if (refreshError?.preparationTimedOut === true) throw refreshError;
      throw error;
    }
    checkAccount(fresh?.account);
    if (
      account?.apiKeySource &&
      fresh.account.apiKeySource !== account.apiKeySource
    )
      throw error;
    if (
      typeof account?.subscriptionType === "string" &&
      account.subscriptionType.trim() &&
      fresh.account.subscriptionType !== account.subscriptionType
    )
      throw Object.assign(
        new Error("Claude Code subscription changed. No input was submitted."),
        {
          claudeAccountValidationFailed: true,
          data: {
            turnStartOutcome: "not_applied",
            claudePreparationFailure: "account_validation",
            claudeAccountFailure: "subscription_mismatch",
          },
        },
      );
    return fresh.account;
  }
}
async function probe(cwd, read, phase = "metadata") {
  const startedAt = Date.now();
  const deadline = startedAt + INITIALIZATION_TIMEOUT_MS;
  let release;
  const hold = new Promise((r) => (release = r));
  // Keep an idle async iterable open until the account probe completes.
  // oxlint-disable-next-line require-yield
  async function* prompt() {
    await hold;
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), INITIALIZATION_TIMEOUT_MS);
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
    const account = await verifiedAccount(
      q,
      deadline,
      () => controller.abort(),
      startedAt,
      phase + "_account",
    );
    return await boundedPreparation(
      () => read(q, account),
      deadline,
      () => controller.abort(),
      phase + "_read",
      startedAt,
    );
  } finally {
    clearTimeout(timer);
    release();
    q.close();
  }
}
const catalogCache = createCatalogCache({
  load: () =>
    probe(
      null,
      async (q, account) => ({
        models: await q.supportedModels(),
        account,
      }),
      "catalog",
    ),
  isActive: () =>
    [...queries.values()].some((active) => active.turn || active.tasks.size),
});
const catalog = () => catalogCache.read();
function wireThread(s, metadata, includeTurns = true) {
  const id = s?.id || metadata.id;
  return {
    id,
    cwd: metadata.cwd,
    createdAt: metadata.createdAt,
    updatedAt: metadata.updatedAt,
    preview: metadata.preview,
    name: metadata.name,
    historyVersion: createHash("sha256")
      .update(metadata.revision)
      .digest("hex"),
    ...(includeTurns ? { turns: s.turns } : { turns: [] }),
    status: { type: queries.get(id)?.turn ? "active" : "idle" },
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
    let result;
    try {
      result = await request(
        "item/tool/requestUserInput",
        {
          threadId: s.id,
          turnId: turn.id,
          itemId: options.toolUseID,
          questions,
        },
        options.signal,
      );
    } catch (error) {
      if (
        error.message !==
        "Only the orchestrator can ask the user. Send your question with orchestration_message target=lead; the orchestrator decides whether to contact the user."
      )
        throw error;
      return { behavior: "deny", message: error.message };
    }
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
        async (args, extra) => {
          const result = await request(
            "item/tool/call",
            {
              threadId: s.id,
              turnId: getTurn()?.id,
              callId: randomUUID(),
              tool: definition.name,
              arguments: args,
            },
            extra?.signal,
          );
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
// Codex continues a thread on turn/start without input; Claude needs text.
const CONTINUE_TEXT =
  "Continue the previous turn from where it stopped. Check the current state first and do not repeat completed work.";
// Claude API error types that Studio can retry, in Codex error names.
function claudeErrorInfo(error, text) {
  if (error === "overloaded") return "serverOverloaded";
  if (error === "server_error") return "internalServerError";
  if (error === "authentication_failed") return "unauthorized";
  if (
    error === "unknown" &&
    /can't reach the api server|ENOTFOUND|ECONNRESET|ECONNREFUSED|ETIMEDOUT|EAI_AGAIN|connection error|socket hang up|fetch failed/i.test(
      text || "",
    )
  )
    return "httpConnectionFailed";
  return null;
}
async function content(input) {
  if (Array.isArray(input) && !input.length)
    return [{ type: "text", text: CONTINUE_TEXT }];
  const result = [];
  for (const item of input || []) {
    if (item.type === "text") result.push({ type: "text", text: item.text });
    else if (item.type === "localImage") {
      const ext = path.extname(item.path).toLowerCase();
      const media_type = {
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".gif": "image/gif",
        ".webp": "image/webp",
      }[ext];
      if (!media_type) throw new Error("Unsupported image format for Claude");
      // Large screenshots are scaled down; the API removes oversized images.
      const image = await claudeImage(item.path, media_type);
      result.push({
        type: "image",
        source: {
          type: "base64",
          media_type: image.mediaType,
          data: image.bytes.toString("base64"),
        },
      });
    } else throw new Error("Unsupported Claude input: " + item.type);
  }
  return result;
}
function turnEvent(s, turn, method, item, tokenRateUsage, receiptTimes) {
  emit(method, {
    threadId: s.id,
    turnId: turn?.id,
    item,
    ...(tokenRateUsage ? { tokenRateUsage } : {}),
    ...receiptTimes,
  });
}
function finishItem(s, turn, item, tokenRateUsage, receiptTimes) {
  if (!turn) return;
  const i = turn.items.findIndex((old) => old.id === item.id);
  if (i < 0) turn.items.push(item);
  else turn.items[i] = item;
  turnEvent(s, turn, "item/completed", item, tokenRateUsage, receiptTimes);
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
    ...thinkingFlag(
      p.model || s.model,
      native.models,
      s.claude?.thinking,
      live,
    ),
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
  if (
    (error?.preparationTimedOut === true ||
      error?.claudeAccountValidationFailed === true) &&
    error?.data?.turnStartOutcome === "not_applied" &&
    turn.items[0]?.type === "userMessage" &&
    typeof turn.clientUserMessageId === "string" &&
    turn.clientUserMessageId.length > 0 &&
    active.input.values.some(
      (input) =>
        input.studioInputIdentity === turn.clientUserMessageId &&
        input.uuid === turn.items[0].nativeId &&
        input.session_id === (s.nativeId || s.id),
    )
  ) {
    // InputQueue removes the message before yielding it to the SDK.
    turn.startOutcome = "not_applied";
  }
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
      ...(turn.apiErrorInfo && !turn.limitError
        ? { codexErrorInfo: turn.apiErrorInfo }
        : {}),
      ...(turn.startOutcome === "not_applied" &&
      (error?.preparationTimedOut === true ||
        error?.claudeAccountValidationFailed === true) &&
      error?.data?.turnStartOutcome === "not_applied"
        ? {
            data: {
              turnStartOutcome: "not_applied",
              ...(error?.data?.claudePreparationFailure === "account_validation"
                ? {
                    claudePreparationFailure: "account_validation",
                    claudeAccountFailure: error.data.claudeAccountFailure,
                  }
                : {}),
              ...(error?.preparationTimedOut === true &&
              typeof error.data.claudePreparationPhase === "string" &&
              Number.isFinite(error.data.claudePreparationElapsedMs)
                ? {
                    claudePreparationPhase: error.data.claudePreparationPhase,
                    claudePreparationElapsedMs:
                      error.data.claudePreparationElapsedMs,
                  }
                : {}),
            },
          }
        : {}),
      ...turn.limitError,
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
  for (const response of active.responseUsages.values()) {
    const total =
      response.requestUsage.inputTokens +
      response.requestUsage.cachedInputTokens +
      response.requestUsage.cacheWriteInputTokens +
      response.requestUsage.outputTokens;
    emit("thread/tokenUsage/updated", {
      threadId: s.id,
      turnId: turn.id,
      responseId: response.responseId,
      responseOutputTokens: response.responseOutputTokens,
      model: response.model,
      usageSource: "claudeResponse",
      requestUsage: response.requestUsage,
      tokenUsage: {
        total: { totalTokens: total },
        last: response.usage,
        modelContextWindow: s.contextWindow,
      },
    });
  }
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
    const output = result?.usage?.output_tokens;
    const turnOutputDelta =
      Number.isFinite(output) && output >= 0
        ? Math.max(0, output - active.reportedOutput)
        : undefined;
    if (turnOutputDelta !== undefined) {
      active.reportedOutput = output;
      active.reportedTurnOutput += turnOutputDelta;
    }
    emit("thread/tokenUsage/updated", {
      threadId: s.id,
      ...(turnOutputDelta !== undefined
        ? { turnOutputTokens: active.reportedTurnOutput }
        : {}),
      ...(Number.isFinite(active.lastUsage?.outputTokens) &&
      active.lastUsage.outputTokens >= 0
        ? { responseOutputTokens: active.lastUsage.outputTokens }
        : {}),
      turnId: turn.id,
      model: active.lastModel,
      responseId: active.lastMessageId,
      ...(active.lastMessageId &&
      Number.isFinite(active.lastUsage?.outputTokens)
        ? { responseOutputTokens: active.lastUsage.outputTokens }
        : {}),
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
  catalogCache.activityChanged();
  if (!active.tasks.size) active.idleSince = Date.now();
  active.pendingSteers.clear();
  active.assistantBlocks.clear();
  active.responseUsages.clear();
  emit("turn/completed", {
    threadId: s.id,
    turn: {
      id: turn.id,
      status: turn.status,
      error: turn.error,
      ...(turn.startOutcome === "not_applied"
        ? {
            startOutcome: "not_applied",
            clientUserMessageId: turn.clientUserMessageId,
          }
        : {}),
    },
  });
}
async function startSession(s, active, p) {
  const initial = active.turn;
  const startedAt = Date.now();
  const deadline = startedAt + INITIALIZATION_TIMEOUT_MS;
  const source = () =>
    JSON.stringify([
      s.cwd,
      s.nativeId,
      s.started,
      s.model,
      s.approvalPolicy,
      s.sandbox,
      s.claude,
      s.developerInstructions,
      s.dynamicTools,
    ]);
  const initialSource = source();
  let admit;
  const admitted = new Promise((r) => (admit = r));
  let allowed = false;
  // Keep an idle async iterable open until the account probe completes.
  // oxlint-disable-next-line require-yield
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
          ...movePrompt,
        },
        // Studio managed agents replace native subagents, as in Codex threads.
        disallowedTools: ["Agent"],
        permissionMode: mode,
        allowDangerouslySkipPermissions: true,
        ...(p.effort ? { effort: p.effort } : {}),
        settings: await boundedPreparation(
          () => flags(s, p),
          deadline,
          () => {},
          "catalog_flags",
          startedAt,
        ),
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
    active.toolsIdentity = JSON.stringify(s.dynamicTools || []);
    await verifiedAccount(q, deadline, () => q.close(), startedAt);
    if (
      initial?.interrupted ||
      active.input.closed ||
      active.turn !== initial ||
      queries.get(s.id) !== active ||
      active.q !== q ||
      source() !== initialSource
    )
      throw Object.assign(
        new Error("Claude preparation source changed. No input was submitted."),
        {
          claudeAccountValidationFailed: true,
          data: {
            turnStartOutcome: "not_applied",
            claudePreparationFailure: "account_validation",
            claudeAccountFailure: "query_source",
          },
        },
      );
    allowed = true;
    admit();
    active.readyResolve();
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
        catalogCache.activityChanged();
        if (!active.turn)
          active.idleSince = active.tasks.size ? null : Date.now();
        continue;
      }
      if (m.type === "user" && m.isReplay) {
        const item = active.turn?.items.find(
          (i) =>
            i.type === "userMessage" &&
            (i.nativeId === m.uuid ||
              (i.nativeId == null &&
                (i.id === m.uuid || active.turn.id === m.uuid))),
        );
        if (item) {
          item.nativeId = m.uuid;
          active.pendingSteers.delete(item.id);
          active.deferredResult = null;
        }
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
        active.reportedTurnOutput = 0;
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
          turn.limitError = {
            codexErrorInfo: "rateLimitExceeded",
            ...(Number.isFinite(info.resetsAt)
              ? { resetsAt: info.resetsAt }
              : {}),
          };
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
          if (!active.turn)
            active.idleSince = active.tasks.size ? null : Date.now();
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
        if (e.type === "message_start") {
          active.messageId = e.message.id;
          emit("provider/generationStarted", {
            threadId: s.id,
            turnId: turn.id,
            responseId: e.message.id,
          });
        }
        if (
          e.type === "content_block_delta" &&
          ["text_delta", "thinking_delta"].includes(e.delta.type)
        ) {
          const thinking = e.delta.type === "thinking_delta";
          const id = active.messageId + (thinking ? ":thinking" : "");
          const delta = e.delta.text || e.delta.thinking || "";
          const state = assistantBlockState(
            active,
            active.messageId,
            thinking ? "thinking" : "text",
          );
          const index = Number.isInteger(e.index) ? e.index : 0;
          let block = state.streams.get(index);
          if (!block) {
            block = { text: "", complete: false };
            state.streams.set(index, block);
            state.blocks.push(block);
          }
          const separator =
            block.text === "" && state.blocks.indexOf(block) > 0 ? "\n" : "";
          if (!block.complete) block.text += delta;
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
          if (!block.complete) {
            item.text = assistantText(state);
            emit("item/agentMessage/delta", {
              threadId: s.id,
              turnId: turn.id,
              itemId: id,
              delta: separator + delta,
            });
          }
        }
      } else if (m.type === "assistant") {
        if (m.error) {
          const errorText = (m.message.content || [])
            .filter((b) => b.type === "text")
            .map((b) => b.text)
            .join("\n");
          turn.apiErrorInfo = claudeErrorInfo(m.error, errorText);
        }
        active.lastModel = m.message.model || active.lastModel;
        active.lastMessageId = m.message.id || active.lastMessageId;
        const responseUsage = usageTokens(m.message.usage);
        active.lastUsage = responseUsage || active.lastUsage;
        const output = m.message.usage?.output_tokens;
        if (m.message.id && m.message.usage && responseUsage) {
          const raw = m.message.usage;
          const requestUsage = {
            inputTokens: Number.isFinite(raw.input_tokens)
              ? raw.input_tokens
              : 0,
            cachedInputTokens: Number.isFinite(raw.cache_read_input_tokens)
              ? raw.cache_read_input_tokens
              : 0,
            cacheWriteInputTokens: Number.isFinite(
              raw.cache_creation_input_tokens,
            )
              ? raw.cache_creation_input_tokens
              : 0,
            outputTokens: Number.isFinite(raw.output_tokens)
              ? raw.output_tokens
              : 0,
          };
          active.responseUsages.set(m.message.id, {
            responseId: m.message.id,
            responseOutputTokens: requestUsage.outputTokens,
            model: active.lastModel,
            requestUsage,
            usage: responseUsage,
          });
        }
        const tokenRateUsage =
          m.message.id && Number.isFinite(output) && output >= 0
            ? { responseId: m.message.id, outputTokens: output }
            : undefined;
        const mergeBlocks = (kind, field) => {
          const state = assistantBlockState(active, m.message.id, kind);
          for (const [index, block] of m.message.content.entries()) {
            if (block.type !== kind || !block[field]) continue;
            const frame = (m.uuid || JSON.stringify(m.message)) + ":" + index;
            if (state.frames.has(frame)) continue;
            state.frames.add(frame);
            const pending = state.blocks.find(
              (old) => !old.complete && old.text === block[field],
            );
            if (pending) pending.complete = true;
            else state.blocks.push({ text: block[field], complete: true });
          }
          return assistantText(state);
        };
        const text = mergeBlocks("text", "text");
        if (text)
          finishItem(
            s,
            turn,
            {
              id: m.message.id,
              nativeId: m.uuid,
              type: "agentMessage",
              text,
              phase: "commentary",
            },
            tokenRateUsage,
          );
        const thinking = mergeBlocks("thinking", "thinking");
        if (thinking)
          finishItem(
            s,
            turn,
            {
              id: m.message.id + ":thinking",
              nativeId: m.uuid,
              type: "agentMessage",
              text: thinking,
              phase: "commentary",
            },
            tokenRateUsage,
          );
        for (const b of m.message.content.filter(
          (b) => b.type === "tool_use",
        )) {
          const item = nativeItem(b, s.cwd);
          active.tools.set(b.id, item);
          turn.items.push(item);
          turnEvent(s, turn, "item/started", item, tokenRateUsage);
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
    if (queries.get(s.id) === active) {
      queries.delete(s.id);
      catalogCache.activityChanged();
      void sessionStore.evict(s.id);
    }
  }
}
// The SDK reports a closed or aborted Claude process with these messages.
function deadQuery(error) {
  return /process aborted by user|operation aborted|process exited|process terminated|transport is not ready|query (is )?closed/i.test(
    String(error?.message || error),
  );
}
function discardQuery(s, active) {
  active.input.close();
  active.q?.close();
  if (queries.get(s.id) === active) queries.delete(s.id);
  catalogCache.activityChanged();
}
function newActive(turn) {
  const active = {
    turn,
    input: new InputQueue(),
    q: null,
    tasks: new Map(),
    tools: new Map(),
    pendingSteers: new Set(),
    assistantBlocks: new Map(),
    responseUsages: new Map(),
    reportedUsage: 0,
    reportedOutput: 0,
    reportedTurnOutput: 0,
    idleSince: null,
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
  const message = {
    type: "user",
    uuid: nativeUserMessageId(id || turn.id),
    session_id: s.nativeId || s.id,
    parent_tool_use_id: null,
    message: { role: "user", content: blocks },
  };
  Object.defineProperty(message, "studioInputIdentity", { value: id });
  return message;
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
      capabilities: { claudeVersion: 21 },
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
        // Aliases such as "sonnet" name a dated model; spawn accepts either id.
        resolvedModel: model.resolvedModel || null,
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
  if (method === "claude/moveVersions")
    return moveVersions(process.env.STUDIO_CLAUDE_BIN || "claude");
  if (method === "claude/moveIdentity")
    return moveIdentity(process.env.STUDIO_CLAUDE_BIN || "claude");
  if (method === "claude/moveExport") {
    const s = await session(p.threadId);
    if (queries.get(s.id)?.turn || queries.get(s.id)?.tasks.size)
      throw new Error(
        "Finish the Claude turn and background tasks before a move",
      );
    return {
      path: await nativeFile(s.nativeId || s.id, undefined, s.cwd),
      session: { ...structuredClone(s), turns: [] },
    };
  }
  if (method === "claude/moveImport") {
    const existing = await sessionStore
      .metadata(p.session.id)
      .catch(() => null);
    const active = queries.get(p.session.id);
    if (active?.turn || active?.tasks.size)
      throw new Error("The destination Claude session is active");
    if (
      existing &&
      (await session(p.session.id)).nativeId !== p.session.nativeId
    )
      throw new Error(
        "The destination Claude session has a different native identity",
      );
    active?.input.close();
    active?.q?.close();
    queries.delete(p.session.id);
    const s = { ...p.session, cwd: p.cwd };
    await publishNative(
      p.path,
      nativeDestination(s.nativeId || s.id, p.cwd),
      !!existing,
    );
    sessions.set(s.id, s);
    await persist(s);
    return { thread: wireThread(s, await sessionStore.metadata(s.id), false) };
  }
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
  if (method === "claude/diagnostics") {
    const values = [...queries.values()];
    return {
      liveQueries: values.length,
      activeTurns: values.filter((active) => active.turn).length,
      backgroundQueries: values.filter((active) => active.tasks.size).length,
      idleQueries: values.filter((active) => !active.turn && !active.tasks.size)
        .length,
      idleLimitSeconds: idleSeconds,
      sessionCache: sessionStore.stats(),
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
    catalogCache.activityChanged();
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
    catalogCache.activityChanged();
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
    return {
      ...p,
      thread: wireThread(s, await sessionStore.metadata(s.id)),
      model: s.model,
      sandbox: null,
    };
  }
  if (method === "thread/turns/list" || method === "thread/turns/items/list") {
    const s = await session(p.threadId);
    let values;
    if (method === "thread/turns/list")
      values = s.turns.filter((turn) => !pendingTurnReceipts.has(turn.id));
    else {
      const t = s.turns.find((t) => t.id === p.turnId);
      if (!t) throw new Error("Unknown Claude turn");
      values = t.items;
    }
    if (method === "thread/turns/list" && p.sortDirection !== "asc")
      values.reverse();
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
    const page = await sessionStore.listMetadata(p.cursor, p.limit);
    return {
      data: page.data.map((metadata) => ({
        id: metadata.id,
        cwd: metadata.cwd,
        createdAt: metadata.createdAt,
        updatedAt: metadata.updatedAt,
        preview: metadata.preview,
        name: metadata.name,
        historyVersion: createHash("sha256")
          .update(metadata.revision)
          .digest("hex"),
        turns: [],
        status: { type: queries.get(metadata.id)?.turn ? "active" : "idle" },
        modelProvider: "claude",
      })),
      nextCursor: page.nextCursor,
    };
  }
  if (method === "skills/extraRoots/set")
    throw new Error("Claude uses its native skills and MCP configuration");
  if (method === "thread/resume" || method === "thread/read") {
    if (method === "thread/read" && p.includeTurns !== true) {
      const metadata = await sessionStore.metadata(p.threadId);
      return {
        thread: wireThread(null, metadata, false),
        model: metadata.model,
        sandbox: null,
        approvalPolicy: metadata.approvalPolicy,
        activePermissionProfile: metadata.activePermissionProfile,
      };
    }
    const s = await session(p.threadId);
    let reattached = false;
    if (method === "thread/resume") {
      const active = queries.get(s.id);
      const idle = () => {
        const query = queries.get(s.id);
        return (
          !query ||
          (!query.turn &&
            !query.tasks.size &&
            !query.pendingSteers.size &&
            !query.reservingInput &&
            !query.input.values.length)
        );
      };
      if (idle())
        await reconcileHistoricalBash(s, {
          isIdle: idle,
          complete: (turn, item, times) =>
            finishItem(s, turn, item, undefined, times),
          persist,
        });
      // Reattach to a live query without changing its settings or background work.
      // turn/start already steers its current turn or queues the next input.
      if (!active?.turn && !active?.tasks.size) {
        active?.input.close();
        active?.q?.close();
        queries.delete(s.id);
        catalogCache.activityChanged();
        Object.assign(s, p);
        await persist(s);
      } else reattached = true;
    }
    const includeTurns =
      method === "thread/read"
        ? p.includeTurns === true
        : p.excludeTurns !== true;
    return {
      thread: wireThread(s, await sessionStore.metadata(s.id), includeTurns),
      model: s.model,
      sandbox: null,
      approvalPolicy: s.approvalPolicy ?? null,
      activePermissionProfile: s.activePermissionProfile ?? null,
      ...(reattached ? { reattached: true } : {}),
    };
  }
  if (method === "thread/unsubscribe") {
    const active = queries.get(p.threadId);
    if (active?.turn || active?.tasks.size)
      throw new Error("Pause the Claude turn before detaching");
    active?.input.close();
    active?.q?.close();
    queries.delete(p.threadId);
    catalogCache.activityChanged();
    await sessionStore.evict(p.threadId);
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
    return {
      thread: wireThread(
        s,
        await sessionStore.metadata(s.id),
        p.excludeTurns !== true,
      ),
      model: s.model,
      sandbox: null,
      approvalPolicy: s.approvalPolicy ?? null,
      activePermissionProfile: s.activePermissionProfile ?? null,
    };
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
    const matches = s.turns.filter(
      (turn) =>
        turn.clientUserMessageId &&
        turn.clientUserMessageId === p.clientUserMessageId,
    );
    if (
      matches.some(
        (turn) =>
          JSON.stringify(turn.items[0].content) !== JSON.stringify(p.input),
      )
    )
      throw new Error("This message identity has different content");
    const prior = matches.find((turn) => turn.startOutcome !== "not_applied");
    if (prior) {
      if (prior.startOutcome === "preparing")
        throw Object.assign(new Error("Claude input receipt outcome unknown"), {
          data: { turnStartOutcome: "unknown" },
        });
      return { turn: { id: prior.id, status: prior.status } };
    }
    const running = queries.get(s.id)?.turn;
    if (running) {
      await handle("turn/steer", {
        threadId: s.id,
        expectedTurnId: running.id,
        clientUserMessageId: p.clientUserMessageId,
        input: p.input,
      });
      return {
        turn: { id: running.id, status: running.status },
        steered: true,
      };
    }
    let nativeCommand;
    if (p.claudeCommand) {
      const commands = await handle("claude/commands", { cwd: s.cwd });
      nativeCommand = explicitNativeCommand(p.claudeCommand, commands);
      // Unknown slash text remains ordinary user input; never extract commands from it.
    }
    const blocks = await content(
      nativeCommand ? [{ type: "text", text: nativeCommand }] : p.input,
    );
    const turnId = randomUUID();
    const turn = {
      id: turnId,
      clientUserMessageId: p.clientUserMessageId,
      startOutcome: "preparing",
      status: "inProgress",
      items: [
        {
          id: p.clientUserMessageId || randomUUID(),
          type: "userMessage",
          content: p.input,
          nativeId: nativeUserMessageId(p.clientUserMessageId || turnId),
        },
      ],
    };
    let active = queries.get(s.id);
    const startedAt = Date.now();
    const deadline = startedAt + PREPARATION_TIMEOUT_MS;
    const control = (read, phase) =>
      boundedPreparation(
        read,
        deadline,
        () => {
          if (
            active &&
            queries.get(s.id) === active &&
            !active.turn &&
            !active.tasks.size
          )
            discardQuery(s, active);
        },
        phase,
        startedAt,
      );
    const requestedTools = p.dynamicTools || s.dynamicTools || [];
    const toolsIdentity = JSON.stringify(requestedTools);
    const toolsChanged = active && active.toolsIdentity !== toolsIdentity;
    s.dynamicTools = requestedTools;
    if (active && !active.turn && !active.tasks.size) {
      // A persistent query can die between turns (its Claude process ends).
      // Nothing is running in it, so start this turn in a fresh query.
      try {
        await control(() => active.ready, "active_ready");
      } catch (error) {
        if (!deadQuery(error)) throw error;
        discardQuery(s, active);
        active = null;
      }
    }
    if (active) {
      await control(() => active.ready, "active_ready");
      if (toolsChanged) {
        // Replace only the Studio MCP server; background tasks keep running.
        // The SDK keeps an already registered in-process server even when its
        // tools change. Remove it first so the new schema replaces it.
        await control(() => active.q.setMcpServers({}), "mcp_remove");
        await control(
          () =>
            active.q.setMcpServers({
              studio: studioTools(s, () => active.turn),
            }),
          "mcp_install",
        );
        active.toolsIdentity = toolsIdentity;
      }
      try {
        await control(() => active.q.setModel(p.model || s.model), "model_set");
        await control(
          () => active.q.setPermissionMode(permissionMode(s, p)),
          "permissions_set",
        );
        const flagSettings = await control(
          () => flags(s, p, true),
          "catalog_flags",
        );
        await control(
          () => active.q.applyFlagSettings(flagSettings),
          "flag_settings",
        );
      } catch (error) {
        if (!deadQuery(error) || active.turn || active.tasks.size) throw error;
        discardQuery(s, active);
        active = null;
      }
    }
    if (active) {
      if (queries.get(s.id) !== active || active.input.closed)
        throw new Error("Claude closed before this turn could start");
      if (active.turn)
        throw new Error(
          "Claude background work started a turn; wait or steer that turn",
        );
      active.turn = turn;
      active.reportedTurnOutput = 0;
      active.idleSince = null;
      active.reservingInput = true;
    }
    s.model = p.model || s.model;
    pendingTurnReceipts.add(turn.id);
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
      pendingTurnReceipts.delete(turn.id);
      throw error;
    }
    if (active) {
      active.reservingInput = false;
      if (active.turn !== turn || active.input.closed) {
        const error = new Error("Claude stopped before accepting the new turn");
        turn.status = "failed";
        turn.startOutcome = "not_applied";
        turn.error = { message: error.message };
        pendingTurnReceipts.delete(turn.id);
        await persist(s);
        throw Object.assign(error, {
          data: { turnStartOutcome: "not_applied" },
        });
      }
      active.lastUsage = null;
    } else {
      active = newActive(turn);
      queries.set(s.id, active);
      setImmediate(() => void startSession(s, active, p));
    }
    catalogCache.activityChanged();
    emit("turn/started", {
      threadId: s.id,
      turn: { id: turn.id, status: "inProgress" },
    });
    active.input.push(
      userMessage(s, turn, blocks, p.clientUserMessageId || turn.id),
    );
    turn.startOutcome = "accepted";
    try {
      await persist(s);
    } catch (error) {
      throw Object.assign(
        new Error(
          "Claude accepted input; receipt persistence failed; outcome unknown",
          { cause: error },
        ),
        { data: { turnStartOutcome: "unknown" } },
      );
    } finally {
      pendingTurnReceipts.delete(turn.id);
    }
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
        nativeId: nativeUserMessageId(id),
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
    // Match Codex: an interrupt without an active turn changes nothing.
    // Closing an idle session here would also end its background tasks.
    if (!active?.turn) throw new Error("no active turn to interrupt");
    if (p.turnId && p.turnId !== active.turn.id)
      throw new Error("The interrupt belongs to a different Claude turn");
    active.turn.interrupted = true;
    active.input.close();
    active.q?.close();
    return {};
  }
  if (method === "thread/name/set") {
    const s = await session(p.threadId);
    if (typeof p.name !== "string" || !p.name.trim())
      throw new Error("Thread name must not be empty");
    s.name = p.name;
    await persist(s);
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
  if (method === "skills/list") return listSkills(p?.cwds);
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
      if (message.error) waiting.reject(new Error(message.error.message));
      else waiting.resolve(message.result);
    }
    return;
  }
  const params = message.params || {};
  // Serialize session mutations, but keep interrupt and permission replies independent.
  const key = params.threadId;
  const ordered =
    key &&
    !(message.method === "thread/read" && params.includeTurns !== true) &&
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
    await sessionStore.drain();
    process.exit(0);
  })();
  return closing;
}
lines.on("close", shutdown);
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
