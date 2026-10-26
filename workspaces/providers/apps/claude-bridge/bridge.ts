import type {
  BridgeMethod,
  BridgeResult,
  BridgeRequest,
  BridgeResponse,
  BridgeRpcId,
  BridgeServerMethod,
  BridgeServerParams,
  BridgeServerResult,
  BridgeInput,
  BridgeMoveProof,
  BridgeTurnParams,
  BridgeSessionParams,
  BridgeTask,
} from "./bridge-protocol.ts";
import type { CanUseTool } from "@anthropic-ai/claude-agent-sdk";
import type {
  Query,
  Options,
  SDKResultMessage,
  SDKUserMessage,
  SDKMessage,
  ModelInfo,
} from "@anthropic-ai/claude-agent-sdk";
import type {
  Session,
  Turn,
  Item,
  Thread,
  SessionMetadata,
} from "./session-state.ts";
import type { InputGroup, StudioInputMessage } from "./input-messages.ts";
import type { Limits } from "./features.ts";
import type { Stall, ToolClock } from "./stall-watch.ts";

type TaskMessage = Extract<SDKMessage, { subtype: `task_${string}` }>;
function isTaskMessage(message: SDKMessage): message is TaskMessage {
  return message.type === "system" && message.subtype.startsWith("task_");
}
import type { CheckedAccount } from "./account-identity.ts";
type SessionContext = Pick<Session, "cwd"> & Partial<Session>;
type AssistantBlock = { text: string; complete: boolean; position?: number };
type AssistantBlockState = {
  blocks: AssistantBlock[];
  streams: Map<number, AssistantBlock>;
  frames: Set<string>;
  text: string;
  dirty: boolean;
  itemTurn: Turn | null;
  itemIndex: number;
};
export type BackgroundTask = BridgeTask;
type QueryStall = {
  turnId: string;
  ended: () => void;
  closed: Promise<void> | null;
  closeError?: unknown;
};
export interface ActiveQuery {
  turn: Turn | null;
  input: InputQueue<StudioInputMessage>;
  q: Query | null;
  tasks: Map<string, BackgroundTask>;
  tools: Map<string, Item>;
  pendingSteers: Set<string | undefined>;
  assistantBlocks: Map<string, AssistantBlockState>;
  usage: ResponseUsage;
  idleRequestStartedAt: number;
  lastEventAt: number;
  toolClocks: Map<string, ToolClock>;
  stall: QueryStall | null;
  ready: Promise<void>;
  readyResolve: () => void;
  readyReject: (error: unknown) => void;
  consumer?: Promise<void>;
  accountProof?: CheckedAccount;
  accountProofQuery?: Query;
  commands?: string[];
  deferredResult?: SDKResultMessage | null;
  resultTimer?: ReturnType<typeof setTimeout> | null;
  stopTimer?: ReturnType<typeof setTimeout> | null;
  finalization?: Promise<void> | null;
  session?: Session;
  externalMcpServers?: Options["mcpServers"];
  permissionMode?: Options["permissionMode"];
  toolsIdentity?: string;
  persistFailure?: unknown;
  reservingInput?: boolean;
  lastModel?: string | undefined;
  messageId?: string;
  idleRequest?: { id: string; at: number };
}

import { ResponseUsage, cumulativeUsage } from "./usage.ts";
import { isThreadSerialized } from "./provider-method-policy.generated.ts";
import {
  cacheReport,
  idleRefusal,
  compactionEnabled,
} from "./idle-compaction.ts";
import {
  prepareFileRestore,
  verifyFileRestore,
  fileBackups,
} from "./file-restore.ts";
import {
  fileCheckpointOptions,
  fileRestoreReason,
  rewindSessionFiles,
} from "./file-checkpoints.ts";
import { createAccountCheck } from "./account-identity.ts";
import { readHistoryIndex, verifiedHistoryMessages } from "./history-index.ts";
import {
  persistNativeSession,
  guardNativeSessionInput,
  cancelNativeSessionInput,
  nativeInputAttempt,
  newNativeInputAttempt,
  guardNativeSessionStart,
  assertNativeSessionReady,
  sessionStartHook,
} from "./session-identity.ts";
import {
  inputGroups,
  sdkInputMessages,
  nativeInputIds,
} from "./input-messages.ts";
// Claude Agent SDK adapter for Studio's existing local session protocol.
import { interruptInputQueue } from "./stop-inputs.ts";
import { browserEnvironment } from "./browser-environment.ts";
import {
  query,
  resolveSettings,
  createSdkMcpServer,
  tool,
  forkSession,
} from "@anthropic-ai/claude-agent-sdk";
import {
  InputQueue,
  usageLimits,
  limitEvent,
  nativeItem,
  overageAllowed,
  claudeErrorInfo,
  providerNotice,
  limitApplies,
} from "./features.ts";
import {
  commandCatalog,
  explicitNativeCommand,
  rollbackSession,
  forkAtTurn,
} from "./controls.ts";
import { z } from "zod";
import { createHash, randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
import fs from "node:fs/promises";
import path from "node:path";
import { stripVTControlCharacters } from "node:util";
import { createCommandTransport, commandMethods } from "./commands.ts";

import { thinkingFlag } from "./thinking.ts";
import {
  createSessionStore,
  ephemeralSessionOptions,
  isDiskFull,
} from "./session-store.ts";
import { claudeImage } from "./images.ts";
import { listSkills } from "./skills.ts";
import { reconcileHistoricalBash } from "./historical-bash-receipts.ts";
import { createCatalogCache, loadCatalog } from "./catalog-cache.ts";
import {
  declaredTimeoutMs,
  durationText,
  watchStalls,
  stallLimits,
  stallMessage,
} from "./stall-watch.ts";
import {
  foregroundCommandUpdate,
  backgroundedCommandFields,
  moveCommandToBackground,
} from "./command-background.ts";

import {
  movePrompt,
  moveIdentity,
  moveVersions,
  nativeFile,
  exactSession,
  exactProof,
  nativeDestination,
  publishNative,
  savedPromptProof,
  verifyToolProof,
  studioToolCatalog,
  exactTurnTools,
} from "./move.ts";

import {
  externalAliases,
  externalCatalog,
  targetExternalCatalog,
  externalRefusal,
  serverAlias,
  canonicalJson,
} from "./move-external.ts";

const externalConfigs = new Map();

type BridgeProviderOptions = Pick<
  import("./runtime-contract.generated.ts").ClaudeSessionSettings,
  "autoCompactWindow" | "customModels"
> &
  Pick<Options, "extraArgs">;
function readProviderOptions(text: string): BridgeProviderOptions {
  const value: unknown = JSON.parse(text);
  if (!isRecord(value)) throw new Error("Invalid Claude bridge options");
  // Rust bridge_options owns this same-program configuration envelope; field
  // types come from its generated settings and the SDK extra-argument owner.
  return value as BridgeProviderOptions;
}
const providerOptions = readProviderOptions(
  process.env.STUDIO_CLAUDE_OPTIONS || "{}",
);
const STUDIO_INPUT_NAMESPACE = "8d95e191-763a-4ee2-a462-7d27f981f138";
function nativeUserMessageId(id: string): NonNullable<SDKUserMessage["uuid"]> {
  if (
    /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/i.test(
      id,
    )
  )
    return id as NonNullable<SDKUserMessage["uuid"]>;
  const namespace = Buffer.from(
    STUDIO_INPUT_NAMESPACE.replaceAll("-", ""),
    "hex",
  );
  const bytes = createHash("sha1")
    .update(namespace)
    .update(String(id))
    .digest()
    .subarray(0, 16);
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x50;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = bytes.toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
function assistantBlockState(
  active: ActiveQuery,
  messageId: string,
  kind: string,
) {
  const key = messageId + ":" + kind;
  if (!active.assistantBlocks.has(key))
    active.assistantBlocks.set(key, {
      blocks: [],
      streams: new Map(),
      frames: new Set(),
      // The joined text is built when read, at most once per change, not per delta.
      text: "",
      dirty: false,
      // The streaming item's index in its turn, so a delta finds it in O(1).
      itemTurn: null,
      itemIndex: -1,
    });
  const state = active.assistantBlocks.get(key);
  if (!state) throw new Error("Assistant block state missing");
  return state;
}
const streamingItems = new WeakSet<Item>();
function assistantText(state: AssistantBlockState) {
  if (state.dirty) {
    state.text = state.blocks
      .map((block) => block.text)
      .filter(Boolean)
      .join("\n");
    state.dirty = false;
  }
  return state.text;
}
// A streaming item reads its text from the block state, so a delta costs O(1)
// and the full text is joined only when the item is saved or sent. Assigning
// `text` (as a finished copy does) turns it back into a plain value.
function streamText(item: Item, state: AssistantBlockState) {
  Object.defineProperty(item, "text", {
    enumerable: true,
    configurable: true,
    get: () => assistantText(state),
    set(value: string) {
      streamingItems.delete(item);
      Object.defineProperty(item, "text", {
        value,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    },
  });
}
let lastLimits: Limits | undefined;
const root = process.argv[2] || "";
if (!root) throw new Error("Claude bridge requires its own state directory");
await fs.mkdir(path.join(root, "sessions"), { recursive: true, mode: 0o700 });
const queries = new Map<string, ActiveQuery>(),
  pending = new Map<
    BridgeRpcId,
    { resolve: (line: string) => void; reject: (error: unknown) => void }
  >();
const pendingTurnReceipts = new Set<string>();
const PREPARATION_TIMEOUT_MS = 20_000;
const PASSIVE_ACCOUNT_TIMEOUT_MS = 8_000;
// A cold Claude process can need more than 20 seconds under launchd limits.
// Retained query controls keep their shorter, shared deadline.
const INITIALIZATION_TIMEOUT_MS = 60_000;

async function boundedPreparation<T>(
  read: () => T | Promise<T>,
  deadline: number,
  onTimeout: () => void,
  phase: string,
  startedAt: number,
): Promise<T> {
  let timer;
  try {
    return await Promise.race([
      Promise.resolve().then(read),
      new Promise<never>((_, reject) => {
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
    clearTimeout(timer ?? undefined);
  }
}
const sessionStore = createSessionStore(root, {
  isPinned: (id: string) => queries.has(id),
});
const sessions = sessionStore.sessions;
// The server closes a session with thread/unsubscribe as soon as its turn
// settles, which exits its CLI; the next turn resumes it. A session with a
// turn, background task, pending input, or serialized operation refuses to close.
// Threads Studio itself unloaded in this process. The CLI's resume_return
// dialog (summary or full history) on their next turn is Studio's own resume,
// not a user's return to a stale chat: it keeps the full history and never asks.
const studioUnloaded = new Set<string>();
const queryHasInput = (active: ActiveQuery) =>
  Boolean(
    active.pendingSteers.size ||
    active.reservingInput ||
    active.input.values.length ||
    active.input.pending.length,
  );
const queryBusy = (id: string, active: ActiveQuery) =>
  Boolean(
    active.turn ||
    active.tasks.size ||
    queryHasInput(active) ||
    operations.get(id),
  );
// The one stdout writer. Lines of one event-loop turn leave in one write at
// the end of that turn, so a burst of events costs one syscall. When stdout
// reports a full buffer, later lines wait in order for "drain", and the SDK
// stream pauses (`stdoutRoom`) while more than the budget waits, so a slow
// reader bounds the bridge's memory instead of growing it.
// UTF-16 code units of queued lines plus bytes buffered by the stream.
const STDOUT_PENDING_BUDGET = 8 * 1024 * 1024;
let stdoutLines: string[] = [];
let stdoutQueued = 0;
let stdoutScheduled = false;
let stdoutDraining = false;
let stdoutRoomWaiters: (() => void)[] = [];
const stdoutPending = () => stdoutQueued + process.stdout.writableLength;
function flushStdout() {
  stdoutScheduled = false;
  if (!stdoutDraining && stdoutLines.length) {
    const data = stdoutLines.join("");
    stdoutLines = [];
    stdoutQueued = 0;
    if (!process.stdout.write(data)) {
      stdoutDraining = true;
      process.stdout.once("drain", () => {
        stdoutDraining = false;
        flushStdout();
      });
    }
  }
  if (stdoutRoomWaiters.length && stdoutPending() <= STDOUT_PENDING_BUDGET) {
    const waiters = stdoutRoomWaiters;
    stdoutRoomWaiters = [];
    for (const resolve of waiters) resolve();
  }
}
// Resolves once the queued output is within the budget.
function stdoutRoom() {
  if (stdoutPending() <= STDOUT_PENDING_BUDGET) return null;
  return new Promise<void>((resolve) => stdoutRoomWaiters.push(resolve));
}
// Linux pipes write synchronously, so queued lines still leave on exit.
process.on("exit", () => {
  if (stdoutLines.length) process.stdout.write(stdoutLines.join(""));
});
const send = (value: unknown) => {
  const line = JSON.stringify(value) + "\n";
  stdoutLines.push(line);
  stdoutQueued += line.length;
  if (!stdoutScheduled && !stdoutDraining) {
    stdoutScheduled = true;
    setImmediate(flushStdout);
  }
};
const emit = (method: string, params: unknown) => send({ method, params });
const commands = createCommandTransport({ root, emit });
const limits = stallLimits();
// Outstanding Studio requests per thread. While Studio owns a wait (an
// approval, a question, a Studio tool call), no Claude tool call can stall.
const studioWaits = new Map<string, number>();
function studioWait(threadId: string | undefined) {
  if (!threadId) return () => {};
  studioWaits.set(threadId, (studioWaits.get(threadId) || 0) + 1);
  armStallWatch();
  let open = true;
  return () => {
    if (!open) return;
    open = false;
    const left = (studioWaits.get(threadId) ?? 0) - 1;
    if (left > 0) studioWaits.set(threadId, left);
    else studioWaits.delete(threadId);
    // The tool budget starts again once Studio answers.
    const active = queries.get(threadId);
    if (!active) return;
    const now = Date.now();
    active.lastEventAt = now;
    for (const clock of active.toolClocks.values())
      clock.startedAt = Math.max(clock.startedAt, now);
    armStallWatch();
  };
}
const request = <M extends BridgeServerMethod>(
  method: M,
  params: BridgeServerParams<M>,
  signal?: AbortSignal,
  onSent?: (id: string) => void,
): Promise<BridgeServerResult<M>> =>
  new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(new Error("Claude request cancelled"));
    const id = "claude:" + randomUUID();
    const settled = studioWait(params?.threadId);
    const abort = () => {
      pending.delete(id);
      settled();
      reject(new Error("Claude request cancelled"));
    };
    const cleanup = () => {
      settled();
      signal?.removeEventListener("abort", abort);
    };
    pending.set(id, {
      resolve: (line) => {
        cleanup();
        const reply = readBridgeInput<M>(line);
        if (!("method" in reply) && "result" in reply) resolve(reply.result);
      },
      reject: (error) => {
        cleanup();
        reject(error);
      },
    });
    signal?.addEventListener("abort", abort, { once: true });
    send({ id, method, params });
    onSent?.(id);
  });
const persist = sessionStore.persist;
// A full disk must not end a running turn: the session stays in memory and the
// store saves it again when space returns. One notice in the turn says so.
const DISK_FULL_TURN_NOTICE =
  "Disk is full. Studio keeps this turn in memory and saves it when free space returns.";
const DISK_FULL_INPUT_ERROR =
  "Disk is full. Claude did not receive this input. Studio sends it when free space returns.";
async function persistLive(s: Session, turn?: Turn | null) {
  if (!(await sessionStore.persistOrDefer(s)) || !turn) return;
  const id = "disk-full:" + turn.id;
  if (turn.items.some((item) => item.id === id)) return;
  finishItem(s, turn, {
    id,
    type: "agentMessage",
    phase: "commentary",
    studioNotice: true,
    text: DISK_FULL_TURN_NOTICE,
  });
  process.stderr.write(
    "Claude session save deferred: disk is full " + s.id + "\n",
  );
}
// Save while the SDK stream continues. The store keeps one session's saves in
// order and joins saves that have not started, and a turn's end waits for them
// through its own save. A failed save ends the stream at its next message.
function persistLater(s: Session, active: ActiveQuery) {
  persistLive(s, active.turn).catch((error) => {
    active.persistFailure ??= error;
  });
}
// The durable pre-input receipt could not be written, so no input reached Claude.
function diskFullInputError(error: unknown) {
  return Object.assign(new Error(DISK_FULL_INPUT_ERROR, { cause: error }), {
    data: {
      turnStartOutcome: "not_applied",
      inputSubmitted: false,
      diskFull: true,
    },
  });
}
async function session(id: string): Promise<Session> {
  return sessionStore.get(id);
}
const settings = (s: Pick<Session, "cwd"> & Partial<Session>): Options =>
  fileCheckpointOptions({
    cwd: s.cwd,
    pathToClaudeCodeExecutable: process.env.STUDIO_CLAUDE_BIN || "claude",
    settingSources: s.moveProof ? [] : ["user", "project", "local"],
    ...(s.moveProof
      ? { strictMcpConfig: true, tools: s.moveProof.options.tools }
      : {}),
    // A managed worker runs without the user's MCP servers (Studio sends userMcp: false).
    ...(s.userMcp === false ? { strictMcpConfig: true } : {}),
    includePartialMessages: true,
    env: {
      ...process.env,
      // Per-thread host declarations must reach the CLI process, not settings.json.
      ...Object.fromEntries(
        ["CLAUDE_CODE_HOST_WORKTREE", "CLAUDE_CODE_HOST_WORKTREE_FENCE"]
          .filter(
            (key) =>
              typeof (isRecord(s.claude?.env)
                ? s.claude.env[key]
                : undefined) === "string",
          )
          .map((key) => [key, s.claude?.env?.[key]]),
      ),
      ...browserEnvironment(root, s.id),
      ...(s.moveProof
        ? {
            ENABLE_CLAUDEAI_MCP_SERVERS: s.moveProof.externalCatalog?.some(
              (server) => server.claudeAI,
            )
              ? "true"
              : "false",
            CLAUDE_CODE_AUTO_CONNECT_IDE: "0",
          }
        : {}),
      ...(s.userMcp === false
        ? {
            ENABLE_CLAUDEAI_MCP_SERVERS: "false",
            CLAUDE_CODE_AUTO_CONNECT_IDE: "0",
          }
        : {}),
      ...(typeof s.studioImageWorkspaceTempDir === "string"
        ? { TMPDIR: s.studioImageWorkspaceTempDir }
        : {}),
    },
    stderr: (line: string) => process.stderr.write(line),
  });
const checkAccount = createAccountCheck({
  expectedEmail: process.env.STUDIO_CLAUDE_ACCOUNT,
  expectedIdentity: process.env.STUDIO_CLAUDE_CREDENTIAL_IDENTITY,
  identityPath: path.join(root, "claude-account-identity"),
});
async function verifiedAccount(
  q: Query,
  deadline: number,
  onTimeout: () => void,
  startedAt: number,
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
    return checkAccount(account);
  } catch (error) {
    if (
      ![
        "account_metadata",
        "email_metadata",
        "organization_metadata",
        "provider_metadata",
        "subscription_metadata",
      ].some((reason) => reason === errorData(error).claudeAccountFailure) ||
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
      if (errorField(refreshError, "preparationTimedOut") === true)
        throw refreshError;
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
    return checkAccount(fresh.account);
  }
}
async function probe<T>(
  cwd: string | null | undefined,
  read: (q: Query, account: CheckedAccount) => T | Promise<T>,
  phase = "metadata",
  external = false,
): Promise<T> {
  const startedAt = Date.now();
  const deadline = startedAt + INITIALIZATION_TIMEOUT_MS;
  const { promise: hold, resolve: release } = Promise.withResolvers<void>();
  // Keep an idle async iterable open until the account probe completes.
  async function* prompt(): AsyncGenerator<never> {
    await hold;
    yield* [];
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
      strictMcpConfig: !external,
      allowedTools: [],
      env: {
        ...process.env,
        ENABLE_CLAUDEAI_MCP_SERVERS: external ? "true" : "false",
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
    clearTimeout(timer ?? undefined);
    release();
    q.close();
  }
}
// Refresh account metadata on the live query's own process.
async function refreshLiveCatalog(live: ActiveQuery, background: boolean) {
  const q = live.q;
  const prior = live.accountProof;
  if (!q || !prior)
    throw new Error("Fresh Claude account control is unavailable");
  const startedAt = Date.now();
  const deadline =
    startedAt +
    (background
      ? Math.min(PREPARATION_TIMEOUT_MS, PASSIVE_ACCOUNT_TIMEOUT_MS)
      : PREPARATION_TIMEOUT_MS);
  if (typeof q.reinitialize !== "function")
    throw new Error("Fresh Claude account control is unavailable");
  const fresh = await boundedPreparation(
    () => q.reinitialize(),
    deadline,
    () => {},
    "catalog_account_reinitialize",
    startedAt,
  );
  checkAccount(fresh?.account);
  if (
    fresh.account.subscriptionType !== prior.subscriptionType ||
    fresh.account.apiKeySource !== prior.apiKeySource
  )
    throw Object.assign(new Error("Claude subscription metadata changed"), {
      claudeAccountValidationFailed: true,
    });
  if (![...queries.values()].includes(live) || live.q !== q)
    throw new Error("Claude query changed during metadata refresh");
  if (!Array.isArray(fresh.models))
    throw new Error("Fresh Claude model metadata is unavailable");
  const verified = checkAccount(fresh.account);
  live.accountProof = verified;
  return { models: fresh.models, account: verified };
}
const catalogCache = createCatalogCache({
  load: (background) =>
    loadCatalog({
      background,
      // Only a query whose current process proved the account can refresh it.
      live: [...queries.values()].find(
        (active) =>
          active.q &&
          active.accountProof &&
          active.accountProofQuery === active.q,
      ),
      refreshLive: (live) => refreshLiveCatalog(live, background),
      probe: () =>
        probe(
          null,
          async (q, account) => ({
            models: await q.supportedModels(),
            account,
          }),
          "catalog",
        ),
    }),
  isActive: () =>
    [...queries.values()].some((active) => active.turn || active.tasks.size),
});
const catalog = () => catalogCache.read();
function wireThread(
  s: Session | null,
  metadata: SessionMetadata,
  includeTurns = true,
): Thread {
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
    ...(includeTurns
      ? {
          turns: s?.turns ?? [],
          ...(s?.journalRepair ? { journalRepair: s.journalRepair } : {}),
        }
      : { turns: [] }),
    status: { type: queries.get(id)?.turn ? "active" : "idle" },
    modelProvider: "claude",
  };
}
async function permissions(
  s: Session,
  turn: Turn | null,
  name: string,
  input: Record<string, unknown>,
  options: Parameters<CanUseTool>[2],
): ReturnType<CanUseTool> {
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
    const rawQuestions = input.questions || [];
    if (!Array.isArray(rawQuestions))
      throw new Error("Invalid Claude question list");
    const questions = rawQuestions.map((q: unknown, i) => {
      if (!isRecord(q)) throw new Error("Invalid Claude question");
      // The native tool schema owns this payload; fields are narrowed before Studio receives it.
      const { question, header, options } = q;
      if (
        typeof question !== "string" ||
        typeof header !== "string" ||
        !Array.isArray(options)
      )
        throw new Error("Invalid Claude question");
      const choices = options.map((option: unknown) => {
        if (
          !isRecord(option) ||
          typeof option.label !== "string" ||
          typeof option.description !== "string"
        )
          throw new Error("Invalid Claude question option");
        return {
          ...option,
          label: option.label,
          description: option.description,
        };
      });
      return {
        ...q,
        question,
        header,
        options: choices,
        id: String(i),
        isOther: true,
        isSecret: false,
      } as const;
    });
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
        errorText(error) !==
        "Only the orchestrator can ask the user. Send your question with orchestration_message target=lead; the orchestrator decides whether to contact the user."
      )
        throw error;
      return { behavior: "deny", message: errorText(error) ?? "" };
    }
    const answers = Object.fromEntries(
      questions.map((q) => [
        q.question,
        (result.answers?.[q.id]?.answers || []).join(", "),
      ]),
    );
    return { behavior: "allow", updatedInput: { ...input, answers } };
  }
  // Claude Code asks even in bypassPermissions mode for ask rules, removals on
  // critical paths and tools that need the user. Such an agent runs without
  // approvals, so nobody watches for this one: decline it when it stays
  // unanswered, instead of blocking the turn and every later message.
  const unexpected = queries.get(s.id)?.permissionMode === "bypassPermissions";
  // The SDK marks the reason render-unsafe: it can carry ANSI escapes.
  const why =
    typeof options.decisionReason === "string"
      ? stripVTControlCharacters(options.decisionReason).trim() || null
      : null;
  const waiting = new AbortController();
  const cancel = () => waiting.abort();
  options.signal?.addEventListener("abort", cancel, { once: true });
  if (options.signal?.aborted) cancel();
  let requestId,
    expired = false;
  const timer = unexpected
    ? setTimeout(() => {
        expired = true;
        waiting.abort();
      }, limits.unexpectedApprovalMs)
    : null;
  let result;
  try {
    result = await request(
      "item/commandExecution/requestApproval",
      {
        threadId: s.id,
        turnId: turn.id,
        itemId: options.toolUseID,
        command:
          name === "Bash"
            ? typeof input.command === "string"
              ? input.command
              : JSON.stringify(input.command)
            : JSON.stringify({ tool: name, input }),
        cwd: s.cwd,
        reason:
          "Claude requests permission to use " +
          name +
          (unexpected ? " although this agent runs without approvals" : "") +
          (why ? ": " + why : ""),
      },
      waiting.signal,
      (id) => (requestId = id),
    );
  } catch (error) {
    if (!expired) throw error;
    // Close Studio's card the way Codex does after a turn transition. A
    // resolved request never counts as an approval.
    emit("serverRequest/resolved", { threadId: s.id, requestId });
    const message =
      "Nobody answered the approval for this " +
      name +
      " call in " +
      durationText(limits.unexpectedApprovalMs) +
      ", so Studio declined it. The command did not run.";
    const owner = queries.get(s.id)?.turn;
    if (owner)
      finishItem(s, owner, {
        id: "approval-expired:" + options.toolUseID,
        type: "agentMessage",
        phase: "commentary",
        studioNotice: true,
        text: message,
      });
    return { behavior: "deny", message };
  } finally {
    clearTimeout(timer ?? undefined);
    options.signal?.removeEventListener("abort", cancel);
  }
  return result.decision === "accept" || result.decision === "acceptForSession"
    ? { behavior: "allow", updatedInput: input }
    : { behavior: "deny", message: "The user declined this action." };
}
function studioTools(
  s: Pick<Session, "dynamicTools"> & Partial<Pick<Session, "id">>,
  getTurn: () => Turn | null,
) {
  return createSdkMcpServer({
    name: "studio",
    version: "1.0.0",
    tools: (s.dynamicTools || []).map((definition) => {
      const schema = z.fromJSONSchema(definition.inputSchema);
      if (!(schema instanceof z.ZodObject))
        throw new Error("Studio dynamic tool requires an object schema");
      return tool(
        definition.name,
        definition.description,
        schema.shape,
        async (args, extra) => {
          if (!s.id) throw new Error("Studio tool has no session");
          const result = await request(
            "item/tool/call",
            {
              threadId: s.id,
              turnId: getTurn()?.id,
              callId: randomUUID(),
              tool: definition.name,
              arguments: args,
            },
            isRecord(extra) && extra.signal instanceof AbortSignal
              ? extra.signal
              : undefined,
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

async function currentExternalProof(
  s: Pick<Session, "cwd" | "moveProof"> & Partial<Pick<Session, "id">>,
  proof: import("./move.ts").MoveProof &
    Partial<Pick<BridgeMoveProof, "externalCatalog" | "externalTransports">>,
) {
  const expected = proof.externalCatalog || s.moveProof?.externalCatalog;
  const aliases = expected
    ? expected.map((server) => server.alias)
    : externalAliases(proof);
  if (!aliases.length) return { catalogs: [], configs: {}, transports: {} };
  if (expected) {
    try {
      return await targetExternalCatalog(s.cwd, expected, {
        transports:
          proof.externalTransports || s.moveProof?.externalTransports || {},
      });
    } catch (error) {
      if (errorData(error).moveRefusal) throw error;
      throw externalRefusal(aliases, [], aliases);
    }
  }
  const read = async (q: Query) => {
    let statuses = await q.mcpServerStatus();
    for (const alias of aliases) {
      const server = statuses.find((row) => serverAlias(row.name) === alias);
      if (server?.status === "pending") await q.reconnectMcpServer(server.name);
    }
    statuses = await q.mcpServerStatus();
    const result = await externalCatalog(statuses, s.cwd, aliases);
    return result;
  };
  try {
    const active = s.id ? queries.get(s.id) : undefined;
    return active?.q
      ? await read(active.q)
      : await probe(s.cwd, read, "teleport_catalog", true);
  } catch (error) {
    if (errorData(error).moveRefusal) throw error;
    throw externalRefusal(aliases, [], aliases);
  }
}
async function movedExternalConfigs(s: Session) {
  if (!s.moveProof?.externalCatalog?.length) return {};
  const key = s.moveProof.optionsHash + ":" + s.cwd;
  if (!externalConfigs.has(key)) await proofOptions(s, s.moveProof);
  return externalConfigs.get(key);
}
async function proofOptions(
  s: SessionContext,
  proof: import("./move.ts").MoveProof,
): Promise<BridgeMoveProof> {
  const compiled = await studioToolCatalog(
    studioTools(s, () => null),
    (s.dynamicTools || []).length,
  );
  const baseline =
    exactProof(proof).compiledTools ||
    (s.moveProof && exactProof(s.moveProof)?.compiledTools);
  const tools = verifyToolProof(
    { ...proof, ...(baseline ? { compiledTools: baseline } : {}) },
    compiled,
  );
  const external = await currentExternalProof(s, proof);
  const options: BridgeMoveProof["options"] = {
    tools,
    ...(proof.inlineTools === false
      ? {
          studioCatalogHash: createHash("sha256")
            .update(JSON.stringify(compiled))
            .digest("hex"),
        }
      : {}),
    ...(external.catalogs.length ? { externalCatalog: external.catalogs } : {}),
    disallowedTools: ["Agent"],
    settingSources: [],
    strictMcpConfig: true,
    model: s.model,
    permissionMode: permissionMode(s, {}),
    settings: await flags(s, { model: s.model, ...s.moveTurnOptions }),
    effort: s.moveTurnOptions?.effort || null,
    systemPrompt: {
      type: "preset",
      preset: "claude_code",
      append: s.developerInstructions || "",
      ...movePrompt,
    },
    extraArgs: providerOptions.extraArgs || {},
  };
  const optionsHash = createHash("sha256")
    .update(canonicalJson(options))
    .digest("hex");
  externalConfigs.set(optionsHash + ":" + s.cwd, external.configs);
  return {
    ...proof,
    ...(proof.inlineTools === false
      ? { compiledTools: compiled, compiledToolsJson: JSON.stringify(compiled) }
      : {}),
    ...(external.catalogs.length ? { externalCatalog: external.catalogs } : {}),
    ...(Object.keys(external.transports || {}).length
      ? { externalTransports: external.transports }
      : {}),
    options,
    optionsHash,
    proofMethod: external.catalogs.length
      ? "saved_snapshot_and_current_ordered_mcp_catalogs"
      : "saved_snapshot_and_identical_studio_options",
  };
}
// Codex continues a thread on turn/start without input; Claude needs text.
const CONTINUE_TEXT =
  "Continue the previous turn from where it stopped. Check the current state first and do not repeat completed work.";
async function content(
  input: BridgeInput[] | null | undefined,
): Promise<InputGroup["blocks"]> {
  if (Array.isArray(input) && !input.length)
    return [{ type: "text", text: CONTINUE_TEXT }];
  const result: InputGroup["blocks"] = [];
  for (const item of input || []) {
    if (item.type === "text") result.push({ type: "text", text: item.text });
    else if (item.type === "localImage") {
      const ext = path.extname(item.path).toLowerCase();
      const mediaTypes: Record<string, string> = {
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".gif": "image/gif",
        ".webp": "image/webp",
      };
      const media_type = mediaTypes[ext];
      if (!media_type) throw new Error("Unsupported image format for Claude");
      // Refuse input before it reaches Claude if portable preprocessing fails.
      const image = await claudeImage(item.path);
      result.push({
        type: "image",
        source: {
          type: "base64",
          media_type: image.mediaType,
          data: image.bytes.toString("base64"),
        },
      });
    } else
      throw new Error("Unsupported Claude input: " + errorField(item, "type"));
  }
  return result;
}
function turnEvent(
  s: Session,
  turn: Turn | null | undefined,
  method: string,
  item: Item,
  receiptTimes?: Record<string, number>,
) {
  emit(method, {
    threadId: s.id,
    turnId: turn?.id,
    item,
    ...receiptTimes,
  });
}
// A submitted steer whose echo arrives after its turn ended.
function submittedSteer(s: Session, nativeId: string | undefined) {
  for (const turn of s.turns.slice(-8).reverse()) {
    const item = turn.items.find(
      (i) =>
        i.type === "userMessage" &&
        i.delivery === "steer" &&
        i.deliveryStatus === "submitted" &&
        i.nativeId === nativeId,
    );
    if (item) return { turn, item };
  }
  return null;
}
function finishItem(
  s: Session,
  turn: Turn | null | undefined,
  item: Item,
  receiptTimes?: Record<string, number>,
) {
  if (!turn) return;
  const i = turn.items.findIndex((old) => old.id === item.id);
  if (i < 0) turn.items.push(item);
  else turn.items[i] = item;
  turnEvent(s, turn, "item/completed", item, receiptTimes);
}
function notice(
  s: Session,
  turn: Turn,
  text: string,
  id: string = randomUUID(),
) {
  finishItem(s, turn, { id, type: "agentMessage", phase: "commentary", text });
}
// Claude refused the turn at a subscription limit. The turn error carries the
// provider's reset time when Claude gave one, so Studio plans the resume for it.
function limitReached(
  s: Session,
  turn: Turn,
  info: import("./features.ts").RateLimitInfo,
) {
  turn.limitError = {
    codexErrorInfo: "rateLimitExceeded",
    ...(Number.isFinite(info.resetsAt) ? { resetsAt: info.resetsAt } : {}),
  };
  notice(
    s,
    turn,
    "Claude usage limit reached (" +
      (info.rateLimitType || "unknown") +
      "). " +
      (info.resetsAt
        ? "Resets at " + new Date(info.resetsAt * 1000).toISOString() + ". "
        : "") +
      "The turn waits. You can pause it.",
    "limit:" + turn.id + ":" + info.rateLimitType + ":" + info.resetsAt,
  );
}
const permissionMode = (
  s: Pick<Session, "claude" | "approvalPolicy" | "sandbox">,
  p: Partial<BridgeSessionParams> = {},
) =>
  s.claude?.permissionMode ||
  ((p.approvalPolicy || s.approvalPolicy) === "never"
    ? "bypassPermissions"
    : s.sandbox === "read-only"
      ? "plan"
      : "default");
async function flags(
  s: Pick<Session, "model" | "claude">,
  p: Partial<BridgeTurnParams> = {},
  live = false,
): Promise<Parameters<Query["applyFlagSettings"]>[0]> {
  const native = await catalog();
  const autoCompactWindow =
    s.claude?.autoCompactWindow || providerOptions.autoCompactWindow;
  return {
    // Studio owns the durable idle timer. Disable the CLI's private timer.
    idleCompaction: false,
    fastMode: p.serviceTier === "priority",
    ...(p.effort !== undefined ? { effortLevel: p.effort } : {}),
    ...thinkingFlag(
      p.model || s.model,
      native.models,
      s.claude?.thinking,
      live,
    ),
    ...(autoCompactWindow ? { autoCompactWindow } : {}),
  };
}
// A turn whose end is being recorded. active.turn still names it until
// finishTurn persists, but it accepts no more input. The value settles when
// finishTurn has cleared the turn, or when recording its end failed.
const finishingTurns = new WeakMap<Turn, Promise<void>>();
function clearDeferredResult(active: ActiveQuery) {
  clearTimeout(active.resultTimer ?? undefined);
  active.resultTimer = null;
  active.deferredResult = null;
}
function clearTurnTimers(active: ActiveQuery) {
  clearDeferredResult(active);
  clearTimeout(active.stopTimer ?? undefined);
  active.stopTimer = null;
}
function lifecycleFailure(s: Session, active: ActiveQuery, error: unknown) {
  active.persistFailure ??= error;
  process.stderr.write(
    "Claude turn finalization failed: " + errorText(error) + "\n",
  );
  // Watchdog failures must end a visible turn, even if the SDK never yields
  // again. A finalizer already owns its completion and cannot be repeated.
  if (!active.turn || finishingTurns.has(active.turn)) return;
  return finishTurn(s, active, null, error).catch((failure) => {
    process.stderr.write(
      "Claude turn finalization failed: " + errorText(failure) + "\n",
    );
  });
}
function deferResult(
  s: Session,
  active: ActiveQuery,
  result: SDKResultMessage,
) {
  active.deferredResult = result;
  if (active.resultTimer) return;
  const turn = active.turn;
  active.resultTimer = setTimeout(() => {
    if (active.turn !== turn || !active.deferredResult) return;
    void finishTurn(s, active, active.deferredResult).catch(
      (error) => s && lifecycleFailure(s, active, error),
    );
  }, limits.steerEchoGraceMs);
  active.resultTimer.unref();
}
function finishTurn(
  s: Session,
  active: ActiveQuery,
  result: SDKResultMessage | null,
  error?: unknown,
): Promise<void> {
  const turn = active.turn;
  if (!turn) return active.finalization || Promise.resolve();
  const finishing = finishingTurns.get(turn);
  if (finishing) return finishing;
  // Shutdown preserves unfinished turns for restart reconciliation. Every
  // terminal caller shares this admission, including SDK errors and timers.
  if (closing) return Promise.resolve();
  const state: {
    closed: boolean;
    stall: QueryStall | null;
    unappliedSteers: (string | undefined)[];
  } = { closed: false, stall: null, unappliedSteers: [] };
  // Install the shared outcome before finalization can await or throw.
  const operation = Promise.resolve().then(async () => {
    try {
      await finalizeTurn(s, active, turn, result, error, state);
    } catch (failure) {
      active.persistFailure = failure;
      turn.status = "failed";
      turn.error = {
        message: "Claude turn finalization failed: " + errorText(failure),
      };
      state.closed = true;
      throw failure;
    } finally {
      clearTurnTimers(active);
      active.toolClocks.clear();
      if (active.turn === turn) active.turn = null;
      if (state.stall) {
        active.stall = null;
        state.stall.ended();
      }
      catalogCache.activityChanged();
      active.pendingSteers.clear();
      active.assistantBlocks.clear();
      if (state.closed) discardQuery(s.id, active);
      emit("turn/completed", {
        threadId: s.id,
        turn: {
          id: turn.id,
          status: turn.status,
          error: turn.error,
          ...(state.unappliedSteers.length
            ? { unappliedSteers: state.unappliedSteers }
            : {}),
          ...(turn.startOutcome === "not_applied"
            ? {
                startOutcome: "not_applied",
                clientUserMessageId: turn.clientUserMessageId,
              }
            : {}),
        },
      });
    }
  });
  finishingTurns.set(turn, operation);
  active.finalization = operation;
  // Every caller still receives this rejection, including callers joining
  // after cleanup. Avoid an unobserved promise while a closed SDK unwinds.
  operation.catch(() => {});
  return operation;
}
async function finalizeTurn(
  s: Session,
  active: ActiveQuery,
  turn: Turn,
  result: SDKResultMessage | null,
  error: unknown,
  state: {
    closed: boolean;
    stall: QueryStall | null;
    unappliedSteers: (string | undefined)[];
  },
) {
  const stall = (state.stall =
    active.stall?.turnId === turn.id ? active.stall : null);
  let closeError = stall?.closeError;
  error ||= closeError;
  let closed = (state.closed = Boolean(stall?.closed));
  if (closed) await stall?.closed;
  // A handed-off steer without an echo cannot coexist with a later turn on
  // this CLI. Stop it before finishing; only its final native history can
  // prove whether the server may redeliver that exact input.
  const unknown = turn.items.some(
    (item) =>
      item.delivery === "steer" &&
      item.deliveryStatus === "submitted" &&
      !active.input.values.some(
        (input) => input.studioInputIdentity === item.id,
      ),
  );
  if (unknown && !closed) {
    closed = state.closed = true;
    active.input.close();
    closeError = closeQuery(active);
    error ||= closeError;
    await new Promise((resolve) => setTimeout(resolve, limits.exitWaitMs));
  }
  clearDeferredResult(active);
  clearTimeout(active.stopTimer ?? undefined);
  active.stopTimer = null;
  active.toolClocks.clear();
  if (
    (errorField(error, "preparationTimedOut") === true ||
      errorField(error, "claudeAccountValidationFailed") === true) &&
    errorData(error).turnStartOutcome === "not_applied" &&
    turn.items[0]?.type === "userMessage" &&
    typeof turn.clientUserMessageId === "string" &&
    turn.clientUserMessageId.length > 0 &&
    // Stop can withdraw the initial input before preparation fails. Its
    // cancellation receipt preserves the proof that Claude did not read it.
    (turn.items[0].deliveryStatus === "cancelled" ||
      active.input.values.some(
        (input) =>
          input.studioInputIdentity === turn.clientUserMessageId &&
          input.uuid === turn.items[0]?.nativeId &&
          input.session_id === (s.nativeId || s.id),
      ))
  ) {
    // InputQueue removes the message before yielding it to the SDK.
    turn.startOutcome = "not_applied";
  }
  if (turn.startOutcome === "not_applied")
    cancelNativeSessionInput(s, nativeInputAttempt(turn, turn.items[0]));
  turn.status = turn.interrupted
    ? "interrupted"
    : error || result?.is_error || result?.subtype !== "success"
      ? "failed"
      : "completed";
  armStallWatch();
  const errorMessage =
    errorText(error) ||
    (result && "errors" in result ? result.errors.join("\n") : undefined) ||
    (result && "result" in result ? result.result : undefined) ||
    "Claude turn failed";
  const errorInfo =
    turn.apiErrorInfo ||
    claudeErrorInfo(
      typeof errorField(error, "error") === "string"
        ? String(errorField(error, "error"))
        : undefined,
      errorMessage,
    );
  if (turn.stallError) turn.error = turn.stallError;
  else if (error || turn.status === "failed")
    turn.error = {
      message: errorMessage,
      ...(errorInfo && !turn.limitError ? { codexErrorInfo: errorInfo } : {}),
      ...(turn.startOutcome === "not_applied" &&
      (errorField(error, "preparationTimedOut") === true ||
        errorField(error, "claudeAccountValidationFailed") === true) &&
      errorData(error).turnStartOutcome === "not_applied"
        ? {
            data: {
              turnStartOutcome: "not_applied",
              ...(errorData(error).claudePreparationFailure ===
              "account_validation"
                ? {
                    claudePreparationFailure: "account_validation",
                    claudeAccountFailure: errorData(error).claudeAccountFailure,
                  }
                : {}),
              ...(errorField(error, "preparationTimedOut") === true &&
              typeof errorData(error).claudePreparationPhase === "string" &&
              Number.isFinite(errorData(error).claudePreparationElapsedMs)
                ? {
                    claudePreparationPhase:
                      errorData(error).claudePreparationPhase,
                    claudePreparationElapsedMs:
                      errorData(error).claudePreparationElapsedMs,
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
        !i.studioNotice &&
        !i.thinking &&
        !i.id?.endsWith(":thinking"),
    )
    .at(-1);
  if (answer) finishItem(s, turn, { ...answer, phase: "final_answer" });
  // Steer input still queued here was never read by Claude. Withdraw it so a
  // later turn cannot consume it, and report it; Studio queues it again once.
  // Input already handed to Claude without an echo has an unknown outcome.
  const unappliedSteers = state.unappliedSteers;
  for (const item of turn.items) {
    if (item.delivery === "steer" && item.deliveryStatus === "cancelled") {
      cancelNativeSessionInput(s, nativeInputAttempt(turn, item));
      unappliedSteers.push(item.id);
      continue;
    }
    if (item.delivery !== "steer" || item.deliveryStatus !== "submitted")
      continue;
    const queued = active.input.values.findIndex(
      (input) => input.studioInputIdentity === item.id,
    );
    if (queued < 0) continue;
    active.input.values.splice(queued, 1);
    item.deliveryStatus = "not_applied";
    cancelNativeSessionInput(s, nativeInputAttempt(turn, item));
    item.error = "Claude finished before reading this steer";
    unappliedSteers.push(item.id);
  }
  if (closed) {
    active.input.withdraw();
    // A failed close cannot prove that the SDK stopped applying input.
    if (!closeError) await reconcileClosedSteers(s, turn, unappliedSteers);
  }
  for (const studioUsage of active.usage.finish("streamInterrupted", turn.id))
    emit("thread/tokenUsage/updated", {
      threadId: s.id,
      turnId: turn.id,
      studioUsage,
    });
  s.updatedAt = Math.floor(Date.now() / 1000);
  await persistLive(s, turn);
  // This save also covers any earlier background save of this turn.
  active.persistFailure = null;
}
// Steer input handed to a CLI that the watchdog closed. The dead process can no
// longer apply it, and Claude Code records a queued message without its text,
// so a resume cannot restore it. Native history decides: recorded input is
// delivered; the rest returns to Studio once as unapplied.
async function reconcileClosedSteers(
  s: Session,
  turn: Turn,
  unapplied: (string | undefined)[],
) {
  const open = turn.items.filter(
    (item) =>
      item.type === "userMessage" &&
      item.delivery === "steer" &&
      item.deliveryStatus === "submitted",
  );
  if (!open.length) return;
  const index = await readHistoryIndex(s);
  const initial = turn.items.find((item) => item.type === "userMessage");
  for (const item of open) {
    const outcome =
      index?.outcome(nativeInputIds(item), nativeInputIds(initial)) ??
      "unknown";
    if (outcome === "delivered") {
      item.deliveryStatus = "delivered";
      turnEvent(s, turn, "item/completed", { ...item, clientId: item.id });
    } else if (outcome === "not_applied") {
      item.deliveryStatus = "not_applied";
      cancelNativeSessionInput(s, nativeInputAttempt(turn, item));
      item.error = "Claude stopped before reading this steer";
      unapplied.push(item.id);
    } else {
      item.error =
        "Claude steer outcome unknown: native history is incomplete or uncertain";
      finishItem(s, turn, {
        id: "uncertain-input:" + item.id,
        type: "agentMessage",
        phase: "commentary",
        studioNotice: true,
        text: item.error,
      });
    }
  }
}
// Interrupt a turn whose tool call stalled; close the CLI if it cannot end the
// turn. The turn ends as interrupted with the stall as its error, so Studio
// shows it and starts the next turn from queued input.
async function recoverStall(
  s: Session | undefined,
  active: ActiveQuery,
  found: (Stall & { stop?: false }) | { stop: true; id: string },
) {
  const turn = active.turn;
  const item = active.tools.get(found.id);
  if (closing || !s || !turn || active.stall || finishingTurns.has(turn))
    return;
  const message = found.stop
    ? "Studio stopped this turn: Claude Code did not end the turn after Stop."
    : stallMessage({ ...found, tool: item?.tool || found.tool });
  const { promise: finished, resolve: ended } = Promise.withResolvers<void>();
  active.stall = { turnId: turn.id, ended, closed: null };
  turn.interrupted = true;
  turn.stallError = {
    message,
    data: found.stop
      ? { claudeStopTimeout: true }
      : {
          claudeToolStall: {
            toolUseId: found.id,
            tool: item?.tool || found.tool,
            timeoutMs: found.timeoutMs,
            elapsedMs: found.elapsedMs,
            silentMs: found.silentMs,
          },
        },
  };
  if (item)
    finishItem(s, turn, {
      ...item,
      status: "failed",
      stalled: true,
      result: { content: message },
      ...(item.type === "commandExecution"
        ? { aggregatedOutput: message, exitCode: null }
        : {}),
    });
  // A notice, not Claude's answer: finishTurn never marks it final.
  finishItem(s, turn, {
    id: "stall:" + found.id,
    type: "agentMessage",
    phase: "commentary",
    studioNotice: true,
    text: message,
  });
  process.stderr.write("Claude stall: " + s.id + " " + message + "\n");
  try {
    await persist(s);
  } catch (error) {
    if (!closing)
      process.stderr.write(
        "Claude stall: persist failed: " + errorText(error) + "\n",
      );
  }
  // A save already in flight may settle after shutdown closes the query.
  if (closing) return;
  let timer;
  const deadline = new Promise(
    (resolve) =>
      (timer = setTimeout(resolve, found.stop ? 0 : limits.interruptWaitMs)),
  );
  // An interrupt keeps queued steers: Claude Code runs them next.
  if (!found.stop)
    Promise.resolve()
      .then(() => {
        if (!closing) return active.q?.interrupt();
      })
      .catch((error) =>
        process.stderr.write(
          "Claude stall: interrupt failed: " + errorText(error) + "\n",
        ),
      );
  await Promise.race([finished, deadline]);
  clearTimeout(timer ?? undefined);
  if (
    closing ||
    active.turn !== turn ||
    finishingTurns.has(turn) ||
    queries.get(s.id) !== active
  )
    return;
  // The CLI did not end the turn. Close it; finishTurn waits for its exit.
  const stall = active.stall;
  stall.closed = new Promise((resolve) =>
    setTimeout(resolve, limits.exitWaitMs),
  );
  active.input.close();
  stall.closeError = closeQuery(active);
  if (stall.closeError)
    turn.stallError = {
      ...turn.stallError,
      message: message + " " + errorText(stall.closeError),
    };
  await stall.closed;
  // Closing normally ends the SDK stream, and its owner finishes the turn.
  // If the stream still has not ended, finish it here and drop the query.
  await new Promise((resolve) => setImmediate(resolve));
  if (closing || active.turn !== turn || finishingTurns.has(turn)) return;
  await finishTurn(
    s,
    active,
    null,
    stall.closeError || new Error("Claude did not stop after Studio closed it"),
  ).catch((error) => lifecycleFailure(s, active, error));
}
const armStallWatch = watchStalls(
  queries,
  studioWaits,
  limits,
  (s, active, found) =>
    recoverStall(s, active, found).catch(
      (error) => s && lifecycleFailure(s, active, error),
    ),
);
async function startSession(
  s: Session,
  active: ActiveQuery,
  p: Partial<BridgeTurnParams>,
) {
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
  const { promise: admitted, resolve: admit } = Promise.withResolvers<void>();
  let allowed = false;
  // Keep an idle async iterable open until the account probe completes.
  async function* prompt() {
    await admitted;
    if (!allowed) return;
    yield* active.input;
  }
  try {
    const externalMcpServers = await movedExternalConfigs(s);
    const titleOptions = ephemeralSessionOptions(s);
    const mode: Options["permissionMode"] =
      titleOptions.permissionMode || permissionMode(s, p);
    await guardNativeSessionStart(
      s,
      persist,
      nativeInputAttempt(initial ?? undefined, initial?.items[0]),
    );
    const effort = s.moveProof ? s.moveProof.options.effort : p.effort;
    const model = s.moveProof?.options.model || p.model || s.model;
    const q = query({
      prompt: prompt(),
      options: {
        ...settings(s),
        // Match Python's --no-session-persistence and --tools "" title flags.
        ...titleOptions,
        ...(model === undefined ? {} : { model }),
        ...(s.started
          ? { resume: s.nativeId || s.id }
          : { sessionId: s.nativeId || s.id }),
        systemPrompt: s.moveProof?.options.systemPrompt || {
          type: "preset",
          preset: "claude_code",
          append: s.developerInstructions || "",
          ...movePrompt,
        },
        // Studio managed agents replace native subagents, as in Codex threads.
        disallowedTools: ["Agent"],
        permissionMode: mode,
        allowDangerouslySkipPermissions: true,
        ...(effort ? { effort } : {}),
        settings: JSON.stringify(
          await boundedPreparation(
            () =>
              s.moveProof
                ? Promise.resolve(s.moveProof.options.settings)
                : flags(s, p),
            deadline,
            () => {},
            "catalog_flags",
            startedAt,
          ),
        ),
        extraArgs: {
          ...providerOptions.extraArgs,
          "replay-user-messages": null,
        },
        canUseTool: (name, input, options) =>
          permissions(s, active.turn, name, input, options),
        mcpServers: {
          ...externalMcpServers,
          studio: studioTools(s, () => active.turn),
        },
        hooks: {
          SessionStart: [
            {
              hooks: [
                sessionStartHook(s, () => active.turn ?? undefined, persist),
              ],
            },
          ],
        },
        supportedDialogKinds: ["resume_return"],
        perTaskStopAffordance: true,
        onUserDialog: async (value, options) => {
          if (value.dialogKind !== "resume_return" || !active.turn)
            return { behavior: "cancelled" };
          if (studioUnloaded.has(s.id))
            return { behavior: "completed", result: "continue" };
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
          const updated =
            answer?.behavior === "allow" ? answer.updatedInput : undefined;
          const answers =
            updated && isRecord(updated.answers) ? updated.answers : undefined;
          const selected = answers?.[question];
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
              request: value,
              ...value,
            },
            options.signal,
          ),
      },
    });
    active.q = q;
    active.externalMcpServers = externalMcpServers;
    active.session = s;
    active.permissionMode = mode;
    active.toolsIdentity = JSON.stringify(s.dynamicTools || []);
    const accountProof = await verifiedAccount(
      q,
      deadline,
      () => q.close(),
      startedAt,
    );
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
    active.accountProof = accountProof;
    active.accountProofQuery = q;
    allowed = true;
    admit();
    active.readyResolve();
    for await (const m of q) {
      const parentToolUseId =
        "parent_tool_use_id" in m ? m.parent_tool_use_id : undefined;
      const room = stdoutRoom();
      if (room) await room;
      // A grace timer can finish a turn during the backpressure wait. Process
      // this message only after that turn's completion has been recorded.
      if (active.turn && finishingTurns.has(active.turn))
        await finishingTurns.get(active.turn);
      if (active.persistFailure) throw active.persistFailure;
      assertNativeSessionReady(s);
      if (queries.get(s.id) !== active) break;
      active.lastEventAt = Date.now();
      armStallWatch();
      if (m.type === "system" && m.subtype === "init") {
        await persistNativeSession(
          s,
          m.session_id,
          "init",
          active.turn ?? undefined,
          persist,
        );
        s.started = true;
        s.claudeVersion = m.claude_code_version;
        active.commands = m.slash_commands || [];
        await persist(s);
        continue;
      }
      if (m.type === "system" && m.subtype === "background_tasks_changed") {
        active.tasks = new Map(m.tasks.map((t) => [t.task_id, t]));
        catalogCache.activityChanged();
        continue;
      }
      if (m.type === "user" && "isReplay" in m && m.isReplay) {
        const activeTurn = active.turn;
        const item = activeTurn?.items.find(
          (i) =>
            i.type === "userMessage" &&
            (i.nativeId === m.uuid ||
              (i.nativeId == null &&
                (i.id === m.uuid || activeTurn.id === m.uuid))),
        );
        if (item && activeTurn) {
          item.nativeId = m.uuid;
          if (
            item === activeTurn.items[0] &&
            !process.env.CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING
          ) {
            activeTurn.nativeMessageId = item.nativeIds?.[0] || m.uuid;
            activeTurn.fileCheckpointMessageId = m.uuid;
            activeTurn.fileCheckpointSource = {
              nativeId: s.nativeId || s.id,
              cwd: s.cwd,
            };
            persistLater(s, active);
          }
          active.pendingSteers.delete(item.id);
          clearDeferredResult(active);
        }
        // Claude echoed this exact steer input: it is delivered. The echo can
        // follow the end of its turn, so earlier turns are searched as well.
        const steer =
          item?.delivery === "steer"
            ? { turn: activeTurn, item }
            : submittedSteer(s, m.uuid);
        if (steer?.item.deliveryStatus === "submitted") {
          steer.item.deliveryStatus = "delivered";
          turnEvent(s, steer.turn, "item/completed", {
            ...steer.item,
            clientId: steer.item.id,
          });
          persistLater(s, active);
        }
        continue;
      }
      if (
        !active.turn &&
        ["assistant", "stream_event"].includes(m.type) &&
        !parentToolUseId
      ) {
        active.turn = {
          id: randomUUID(),
          status: "inProgress",
          items: [],
          synthetic: true,
        };
        active.idleRequestStartedAt = Date.now();
        s.turns.push(active.turn);
        emit("turn/started", {
          threadId: s.id,
          turn: { id: active.turn.id, status: "inProgress" },
        });
      }
      const turn = active.turn || (parentToolUseId ? s.turns.at(-1) : null);
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
          limitApplies(info, active.lastModel || s.model) &&
          !overageAllowed(info)
        )
          limitReached(s, turn, info);
        continue;
      }
      if (m.type === "system") {
        if (
          m.subtype === "status" &&
          m.status === "requesting" &&
          !parentToolUseId
        ) {
          active.idleRequestStartedAt = Date.now();
        }
        if (m.subtype === "compact_boundary" && turn) {
          const item = {
            id: m.uuid || randomUUID(),
            type: "contextCompaction",
            status: "completed",
            compactionMetadata: m.compact_metadata,
          };
          finishItem(s, turn, item);
          persistLater(s, active);
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
            m.content ||
              ("output" in m && typeof m.output === "string"
                ? m.output
                : undefined) ||
              "Claude command completed.",
            m.uuid,
          );
        } else if (isTaskMessage(m)) {
          const id = m.task_id || m.uuid;
          const task = {
            ...active.tasks.get(id),
            ...m,
            ...("patch" in m ? m.patch : {}),
          };
          const command = foregroundCommandUpdate(active, task, m);
          if (command && turn) turnEvent(s, turn, "item/started", command);
          // A terminal patch can arrive before the notification's final output.
          const terminal =
            m.subtype === "task_notification" &&
            ["completed", "failed", "killed", "stopped"].includes(m.status);
          if (terminal) active.tasks.delete(id);
          else active.tasks.set(id, task);
          const taskTurn = turn || s.turns.at(-1);
          if (taskTurn) {
            const item = {
              id: "task:" + id,
              type: "mcpToolCall",
              server: "claude",
              tool: task.task_type || "Background task",
              status: terminal
                ? m.status === "completed"
                  ? "completed"
                  : "failed"
                : "inProgress",
              ...(task.output_file ? { outputFile: task.output_file } : {}),
              arguments: { description: task.description },
              result: {
                content: [
                  {
                    type: "text",
                    text:
                      ("summary" in m ? m.summary : undefined) ||
                      ("last_tool_name" in m ? m.last_tool_name : undefined) ||
                      ("description" in m ? m.description : undefined) ||
                      ("status" in m ? m.status : undefined) ||
                      "Running",
                  },
                ],
              },
            };
            if (terminal) finishItem(s, taskTurn, item);
            else {
              const index = taskTurn.items.findIndex(
                (old) => old.id === item.id,
              );
              if (index < 0) taskTurn.items.push(item);
              else taskTurn.items[index] = item;
              turnEvent(s, taskTurn, "item/started", item);
            }
          }
          persistLater(s, active);
        } else if (turn && providerNotice(m)) {
          notice(s, turn, providerNotice(m) ?? "", m.uuid);
          persistLater(s, active);
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
          notice(
            s,
            turn,
            ("text" in m ? m.text : undefined) ||
              ("content" in m ? m.content : undefined) ||
              ("error" in m ? m.error : undefined) ||
              m.subtype,
            m.uuid,
          );
        continue;
      }
      if (m.type === "result") {
        active.usage.recordModels(m.modelUsage);
        const studioUsage = cumulativeUsage(m, turn?.id);
        if (studioUsage) {
          if (
            "modelContextWindow" in studioUsage &&
            studioUsage.modelContextWindow
          )
            s.contextWindow = studioUsage.modelContextWindow;
          emit("thread/tokenUsage/updated", {
            threadId: s.id,
            turnId: turn?.id,
            studioUsage,
          });
        }
      }
      if (!turn) continue;
      if (m.type === "stream_event" || m.type === "assistant")
        for (const studioUsage of m.type === "stream_event"
          ? active.usage.observe(m, turn.id)
          : active.usage.observeAssistant(m, turn.id))
          emit("thread/tokenUsage/updated", {
            threadId: s.id,
            turnId: studioUsage.turnId,
            studioUsage,
          });
      if (m.type === "assistant")
        for (const block of m.message.content || [])
          if (block.type === "tool_use")
            active.usage.registerTool(
              block.id,
              active.usage.turnFor(parentToolUseId, turn.id),
            );
      if (parentToolUseId) {
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
                id:
                  (m.uuid || errorField(m, "message")) +
                  ":" +
                  block.type +
                  ":" +
                  index,
                type: "agentMessage",
                phase: "commentary",
                nativeSubagent: true,
                text: "Subagent: " + text,
              });
          }
        persistLater(s, active);
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
          active.idleRequest = {
            id: e.message.id,
            at:
              typeof m.ttft_ms === "number" &&
              Number.isFinite(m.ttft_ms) &&
              m.ttft_ms >= 0
                ? Date.now() - m.ttft_ms
                : active.idleRequestStartedAt || Date.now(),
          };
          emit("provider/generationStarted", {
            threadId: s.id,
            turnId: turn.id,
            responseId: e.message.id,
          });
        }
        if (
          e.type === "content_block_delta" &&
          (e.delta.type === "text_delta" || e.delta.type === "thinking_delta")
        ) {
          const thinking = e.delta.type === "thinking_delta";
          const id = active.messageId + (thinking ? ":thinking" : "");
          const delta =
            (e.delta.type === "text_delta" ? e.delta.text : e.delta.thinking) ||
            "";
          const state = assistantBlockState(
            active,
            active.messageId ?? "undefined",
            thinking ? "thinking" : "text",
          );
          const index = Number.isInteger(e.index) ? e.index : 0;
          let block = state.streams.get(index);
          if (!block) {
            block = {
              text: "",
              complete: false,
              position: state.blocks.length,
            };
            state.streams.set(index, block);
            state.blocks.push(block);
          }
          const separator =
            block.text === "" && (block.position ?? 0) > 0 ? "\n" : "";
          if (!block.complete) {
            block.text += delta;
            state.dirty = true;
          }
          if (
            state.itemTurn !== turn ||
            turn.items[state.itemIndex]?.id !== id
          ) {
            state.itemTurn = turn;
            state.itemIndex = turn.items.findIndex((i) => i.id === id);
            if (state.itemIndex < 0) {
              state.itemIndex =
                turn.items.push({
                  id,
                  type: "agentMessage",
                  phase: "commentary",
                  text: "",
                  thinking,
                }) - 1;
            }
          }
          const item = turn.items[state.itemIndex];
          if (!item) throw new Error("Streaming assistant item is missing");
          if (!block.complete) {
            if (!streamingItems.has(item)) {
              streamingItems.add(item);
              streamText(item, state);
            }
            emit("item/agentMessage/delta", {
              threadId: s.id,
              turnId: turn.id,
              itemId: id,
              delta: separator + delta,
            });
          }
        }
      } else if (m.type === "assistant") {
        const assistantMessage = m.message;
        if (m.error) {
          const errorText = (m.message.content || [])
            .filter((b) => b.type === "text")
            .map((b) => b.text)
            .join("\n");
          turn.apiErrorInfo = claudeErrorInfo(m.error, errorText) ?? undefined;
          // A turn that Claude starts itself has no Studio turn when its
          // rate_limit_event arrives. The refused request names the limit.
          const params =
            "api_error_params" in m && isRecord(m.api_error_params)
              ? m.api_error_params
              : {};
          const info = isRecord(params.rate_limit_info)
            ? params.rate_limit_info
            : undefined;
          if (
            m.error === "rate_limit" &&
            info?.status === "rejected" &&
            !parentToolUseId &&
            !turn.limitError
          )
            limitReached(s, turn, info);
        }
        active.lastModel = m.message.model || active.lastModel;
        if (
          turn &&
          !turn.idleCompaction &&
          !parentToolUseId &&
          m.message.id &&
          m.message.usage
        ) {
          const { effective } = await resolveSettings({
            cwd: s.cwd,
            settingSources: settings(s).settingSources ?? [],
          });
          s.idleCache = cacheReport(
            s,
            m.message.usage,
            m.message.id,
            active.idleRequest?.id === m.message.id
              ? active.idleRequest.at
              : active.idleRequestStartedAt || Date.now(),
            effective,
          );
          emit("thread/idleCompaction/updated", {
            threadId: s.id,
            turnId: turn.id,
            report: s.idleCache,
          });
          persistLater(s, active);
        }
        const mergeBlocks = (
          kind: "text" | "thinking",
          field: "text" | "thinking",
        ) => {
          const state = assistantBlockState(active, m.message.id, kind);
          for (const [index, block] of m.message.content.entries()) {
            if (block.type !== kind) continue;
            const text =
              field === "text" && block.type === "text"
                ? block.text
                : field === "thinking" && block.type === "thinking"
                  ? block.thinking
                  : undefined;
            if (!text) continue;
            const frame =
              (m.uuid || JSON.stringify(assistantMessage)) + ":" + index;
            if (state.frames.has(frame)) continue;
            state.frames.add(frame);
            const pending = state.blocks.find(
              (old) => !old.complete && old.text === text,
            );
            if (pending) pending.complete = true;
            else {
              state.blocks.push({ text, complete: true });
              state.dirty = true;
            }
          }
          return assistantText(state);
        };
        const text = mergeBlocks("text", "text");
        if (text)
          finishItem(s, turn, {
            id: m.message.id,
            nativeId: m.uuid,
            type: "agentMessage",
            text,
            phase: "commentary",
          });
        const thinking = mergeBlocks("thinking", "thinking");
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
          if (!isRecord(b.input)) throw new Error("Invalid Claude tool input");
          const input = b.input;
          const item = nativeItem({ ...b, input }, s.cwd);
          active.tools.set(b.id, item);
          active.toolClocks.set(b.id, {
            tool: b.name,
            startedAt: Date.now(),
            timeoutMs: declaredTimeoutMs({ ...b, input }),
          });
          armStallWatch();
          turn.items.push(item);
          turnEvent(s, turn, "item/started", item);
          if (b.name === "TodoWrite")
            emit("turn/plan/updated", {
              threadId: s.id,
              turnId: turn.id,
              plan: Array.isArray(input.todos)
                ? input.todos.map((t: unknown) => ({
                    step: isRecord(t) ? t.content : undefined,
                    status: isRecord(t) ? t.status : undefined,
                  }))
                : [],
            });
        }
        persistLater(s, active);
      } else if (m.type === "user" && Array.isArray(m.message?.content)) {
        for (const b of m.message.content.filter(
          (b) => b.type === "tool_result",
        )) {
          active.toolClocks.delete(b.tool_use_id);
          armStallWatch();
          const old = active.tools.get(b.tool_use_id);
          if (!old) continue;
          const text =
            typeof b.content === "string"
              ? b.content
              : JSON.stringify(b.content);
          const background =
            old.type === "commandExecution" && !b.is_error
              ? backgroundedCommandFields(m.tool_use_result)
              : null;
          const finished = {
            ...old,
            status: b.is_error ? "failed" : "completed",
            result: { content: b.content },
            ...(old.type === "commandExecution"
              ? background
                ? { aggregatedOutput: text, ...background }
                : {
                    aggregatedOutput: text,
                    exitCode: b.is_error ? 1 : 0,
                    ...(old.canMoveToBackground
                      ? { canMoveToBackground: false }
                      : {}),
                  }
              : {}),
          };
          active.tools.set(b.tool_use_id, finished);
          finishItem(s, turn, finished);
        }
        persistLater(s, active);
      } else if (m.type === "result") {
        // A shouldQuery:false prefix produces its own zero-turn receipt.
        // Its UUID is distinct from the final part that owns this turn.
        if (
          m.num_turns === 0 &&
          active.input.nonQueryIds.delete(m.user_message_uuid)
        ) {
          if (!m.is_error) continue;
          // A failed prefix must not leave its remaining parts on this query.
          // Any part already handed to the SDK keeps an uncertain outcome.
          await finishTurn(s, active, m);
          active.input.withdraw();
          break;
        }
        if (
          (active.pendingSteers.size || active.reservingInput) &&
          !m.is_error &&
          !turn.interrupted
        ) {
          deferResult(s, active, m);
          continue;
        }
        await finishTurn(s, active, m);
      }
    }
    drainUsage(s.id, active);
    if (active.turn) throw new Error("Claude ended without a completed turn");
  } catch (error) {
    active.readyReject(error);
    drainUsage(s.id, active);
    if (active.turn) {
      try {
        await finishTurn(s, active, null, error);
      } catch (failure) {
        lifecycleFailure(s, active, failure);
      }
    } else if (active.persistFailure)
      lifecycleFailure(s, active, active.persistFailure);
  } finally {
    allowed = false;
    admit();
    const owned = queries.get(s.id) === active;
    discardQuery(s.id, active);
    if (owned) void sessionStore.evict(s.id);
  }
}
// The SDK reports a closed or aborted Claude process with these messages.
function deadQuery(error: unknown) {
  return /process aborted by user|operation aborted|process exited|process terminated|transport is not ready|query (is )?closed/i.test(
    String(errorText(error) || error),
  );
}
function closeQuery(active: ActiveQuery) {
  try {
    active.q?.close();
  } catch (error) {
    process.stderr.write(
      "Claude query close failed: " + errorText(error) + "\n",
    );
    return error;
  }
}
// One query-close accounting owner; a late SDK unwind cannot publish it twice.
function drainUsage(threadId: string, active: ActiveQuery) {
  for (const studioUsage of active.usage.finish("queryClosed"))
    emit("thread/tokenUsage/updated", {
      threadId,
      turnId: studioUsage.turnId,
      studioUsage,
    });
}
function discardQuery(threadId: string, active: ActiveQuery | undefined) {
  if (!active) return;
  drainUsage(threadId, active);
  clearTurnTimers(active);
  active.input.close();
  closeQuery(active);
  if (queries.get(threadId) === active) {
    queries.delete(threadId);
    armStallWatch();
  }
  catalogCache.activityChanged();
}
function newActive(turn: Turn): ActiveQuery {
  const {
    promise: ready,
    resolve: readyResolve,
    reject: readyReject,
  } = Promise.withResolvers<void>();
  const active: ActiveQuery = {
    ready,
    readyResolve,
    readyReject,
    turn,
    input: new InputQueue<StudioInputMessage>(),
    q: null,
    tasks: new Map(),
    tools: new Map(),
    pendingSteers: new Set(),
    assistantBlocks: new Map(),
    usage: new ResponseUsage(),
    idleRequestStartedAt: Date.now(),
    // Stall watchdog state: the newest CLI message time, the running tool
    // calls of the current turn, and the recovery of a stalled turn.
    lastEventAt: Date.now(),
    toolClocks: new Map(),
    stall: null,
  };

  active.ready.catch(() => {});
  return active;
}
function userMessage(
  s: Session,
  turn: Turn,
  groups: InputGroup[],
  id: string,
): StudioInputMessage {
  turn.nativeSessionId = s.nativeId || s.id;
  const item = turn.items.find((item) => item.id === id) || turn.items[0];
  if (!item) throw new Error("Claude input has no owning item");
  guardNativeSessionInput(s, groups, newNativeInputAttempt(turn, item));
  const messages = sdkInputMessages(
    s.nativeId || s.id,
    id || turn.id,
    groups,
    nativeUserMessageId,
  );
  const message = messages.at(-1);
  const first = messages[0];
  if (!message || !first)
    throw new Error("Claude input has no native messages");
  if (item === turn.items[0]) turn.nativeMessageId = first.uuid;
  if (messages.length > 1) {
    item.nativeIds = messages.map((part) => part.uuid);
    // The queue removes this whole submission before yielding its first part.
    // A partial SDK read is uncertain and must never be reported as unapplied.
    Object.defineProperty(message, "studioInputBatch", { value: messages });
  }
  return message;
}

function handle<M extends BridgeMethod>(
  request: BridgeRequest<M>,
): Promise<BridgeResult<M>>;
async function handle(request: BridgeRequest): Promise<unknown> {
  const { method, params: p } = request;
  if (
    isRecord(p) &&
    typeof p.threadId === "string" &&
    isRecord(p) &&
    typeof p.threadId === "string" &&
    isRecord(p) &&
    typeof p.threadId === "string" &&
    p.threadId &&
    [
      "turn/start",
      "turn/steer",
      "thread/fork",
      "thread/rollback",
      "thread/compact/start",
    ].includes(method)
  )
    assertNativeSessionReady(await session(p.threadId));
  if (commandMethods.has(method))
    return commands.handle(method, isRecord(p) ? p : {});
  if (
    isRecord(p) &&
    typeof p.threadId === "string" &&
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
      capabilities: {
        claudeVersion: 23,
        passiveAccountRead: true,
        activityRead: true,
      },
    };
  if (method === "initialized") return {};
  if (method === "model/list") {
    const native = await catalog();
    const models: ModelInfo[] = [
      ...native.models,
      ...(providerOptions.customModels || [])
        .filter((m) => !native.models.some((n) => n.value === m.id))
        .map((m): ModelInfo => ({
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
  // Whether this process runs a Claude turn or a background task now. The server retires an
  // idle bridge after an update; saved turn records cannot say this, as a turn that a stopped
  // bridge left behind stays in progress there.
  if (method === "claude/activity")
    return {
      busy: [...queries.values()].some(
        (active) => !!active.turn || active.tasks?.size > 0,
      ),
    };
  if (method === "account/read") {
    const { account } =
      p.passive === true ? await catalogCache.readPassive() : await catalog();
    return {
      account: {
        type: "claude",
        email: account.email,
        organizationUuid: account.organizationUuid,
        accountUuid: account.accountUuid,
        _credentialIdentity: account._credentialIdentity,
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
  if (method === "claude/moveProof") {
    const s = await session(p.threadId);
    const file = await nativeFile(s.nativeId || s.id, undefined, s.cwd);
    return proofOptions(s, await savedPromptProof(file, s));
  }
  if (method === "claude/movePreflight") {
    const source = p.proof;
    if (source.session.model !== p.model)
      throw new Error("The saved Claude model differs from preflight");
    const proof = await proofOptions(
      exactSession({ ...source.session, cwd: p.cwd }),
      exactProof(source),
    );
    if (proof.optionsHash !== source.optionsHash)
      throw new Error(
        "The effective target tool options differ from the source proof",
      );
    return {
      snapshotHash: proof.snapshotHash,
      optionsHash: proof.optionsHash,
      proofMethod: proof.proofMethod,
    };
  }
  if (method === "claude/moveExport") {
    const s = await session(p.threadId);
    if (queries.get(s.id)?.turn || queries.get(s.id)?.tasks.size)
      throw new Error(
        "Finish the Claude turn and background tasks before a move",
      );
    const proof = await proofOptions(
      s,
      await savedPromptProof(
        await nativeFile(s.nativeId || s.id, undefined, s.cwd),
        s,
      ),
    );
    const { session: _sessionProof, ...moveProof } = proof;
    return {
      path: await nativeFile(s.nativeId || s.id, undefined, s.cwd),
      session: {
        ...structuredClone(s),
        turns: [],
        moveProof,
        dynamicToolsJson: JSON.stringify(s.dynamicTools),
      },
    };
  }
  if (method === "claude/moveImport") {
    const existing = await sessionStore
      .metadata(p.session.id)
      .catch(() => null);
    const active = queries.get(p.session.id);
    if (active && (active.turn || active.tasks.size || queryHasInput(active)))
      throw new Error("The destination Claude session is active");
    const owner = existing && (await session(p.session.id));
    if (
      owner &&
      (owner.nativeId || owner.id) !== (p.session.nativeId || p.session.id)
    )
      throw new Error(
        "The destination Claude session has a different native identity",
      );
    let s: Session = exactSession({ ...p.session, cwd: p.cwd });
    if (!s.moveProof) throw new Error("Claude move proof is missing");
    s.moveProof = exactProof(s.moveProof);
    const saved = await savedPromptProof(p.path, s);
    if (saved.snapshotHash !== s.moveProof?.snapshotHash)
      throw new Error(
        "The imported saved prompt snapshot differs from preflight",
      );
    const proof = await proofOptions(s, saved);
    if (proof.optionsHash !== s.moveProof.optionsHash)
      throw new Error(
        "The effective target tool options changed before import",
      );
    discardQuery(p.session.id, active);
    await publishNative(
      p.path,
      nativeDestination(s.nativeId || s.id, p.cwd),
      !!existing,
    );
    if (existing) {
      const cached = await session(s.id);
      for (const key of Object.keys(cached))
        Reflect.deleteProperty(cached, key);
      Object.assign(cached, s);
      s = cached;
    } else sessions.set(s.id, s);
    await persist(s);
    return { thread: wireThread(s, await sessionStore.metadata(s.id), false) };
  }
  if (method === "claude/repairReport")
    return sessionStore.repairReport(p.threadId);
  if (method === "claude/fileBackups") {
    const s = await session(p.threadId);
    return fileBackups(s, p.settledRequestIds ?? null, root, persist);
  }
  if (method === "claude/state") {
    const s = await session(p.threadId),
      active = queries.get(s.id);
    return {
      settings: s.claude || {},
      turns: s.turns.map((t) => ({
        id: t.id,
        status: t.status,
        ...(t.error ? { error: t.error } : {}),
        canRestoreFiles: fileRestoreReason(s, t, active) === null,
        restoreFilesUnavailableReason: fileRestoreReason(s, t, active),
        text:
          previewInput(
            t.items.find((i) => i.type === "userMessage")?.content,
          ) || "Claude turn",
      })),
      tasks: [...(active?.tasks.values() || [])],
      version: s.claudeVersion,
      nativeId: s.nativeId || s.id,
      ...(s.nativeSessionTransitionError
        ? {
            nativeSessionOutcome: "unknown",
            nativeSessionError: s.nativeSessionTransitionError,
          }
        : {}),
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
      sessionCache: sessionStore.stats(),
    };
  }
  if (method === "claude/settings") {
    const s = await session(p.threadId),
      active = queries.get(s.id);
    if (active && (active.turn || active.tasks.size || queryHasInput(active)))
      throw new Error("Pause Claude before changing these settings");
    discardQuery(s.id, active);
    s.claude = p.settings;
    await persist(s);
    return { settings: s.claude };
  }
  if (method === "thread/rollback") {
    const s = await session(p.threadId),
      active = queries.get(s.id);
    if (active && (active.turn || active.tasks.size || queryHasInput(active)))
      throw new Error("Pause Claude before rollback");
    discardQuery(s.id, active);
    return rollbackSession(s, p, {
      persist,
      forkSession,
      getSessionMessages: (id, options) =>
        verifiedHistoryMessages({ id, cwd: options?.dir ?? s.cwd }),
      prepareFileRestore: (session, messages, messageId) =>
        prepareFileRestore(session, messages, messageId, root),
      verifyFileRestore,
      rewindFiles: (nativeId, messageId, options) =>
        rewindSessionFiles(query, settings(s), nativeId, messageId, options),
    });
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
  if (request.method === "thread/list") {
    const p = request.params;
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
          !query || (!query.turn && !query.tasks.size && !queryHasInput(query))
        );
      };
      if (idle())
        await reconcileHistoricalBash(s, {
          isIdle: idle,
          complete: (turn, item, times) => finishItem(s, turn, item, times),
          persist,
        });
      // Reattach to a live query without changing its settings or background work.
      // turn/start already steers its current turn or queues the next input.
      if (
        !active ||
        (!active.turn && !active.tasks.size && !queryHasInput(active))
      ) {
        discardQuery(s.id, active);
        Object.assign(
          s,
          p,
          p.dynamicTools
            ? { dynamicTools: exactTurnTools(s, p.dynamicTools) }
            : {},
        );
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
    if (active && queryBusy(p.threadId, active))
      throw new Error("Pause the Claude turn before detaching");
    discardQuery(p.threadId, active);
    if (active) studioUnloaded.add(p.threadId);
    await sessionStore.evict(p.threadId);
    return { status: active ? "unsubscribed" : "notLoaded" };
  }
  if (method === "thread/loaded/list")
    return { data: [...queries.keys()], nextCursor: null };
  if (method === "thread/fork") {
    const old = await session(p.threadId);
    const active = queries.get(old.id);
    if (
      (active?.turn || active?.tasks.size) &&
      (p.lastTurnId == null || p.lastTurnId === old.turns.at(-1)?.id)
    )
      throw new Error(
        "Select an earlier completed Claude turn before branching",
      );
    const s = await forkAtTurn(old, p, {
      forkSession,
      getSessionMessages: (id, options) =>
        verifiedHistoryMessages({ id, cwd: options?.dir ?? old.cwd }),
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
    if (p.idleCompaction) {
      const { effective: options } = await resolveSettings({
        cwd: s.cwd,
        settingSources: settings(s).settingSources ?? [],
      });
      const refusal = idleRefusal(
        s,
        queries.get(s.id),
        p.idleCompaction,
        options,
      );
      const disabled = !compactionEnabled(options);
      const reason = disabled
        ? "compaction_off"
        : options.idleCompaction === false
          ? "idle_compaction_off"
          : refusal;
      if (reason) throw new Error("Idle compaction skipped: " + reason);
    }
    return handle({
      method: "turn/start",
      params: {
        ...p,
        model: s.model,
        clientUserMessageId: p.requestId || randomUUID(),
        input: [{ type: "text", text: "/compact" }],
      },
    });
  }
  if (method === "turn/start") {
    const s = await session(p.threadId);
    const matches = p.clientUserMessageId
      ? s.turns.flatMap((turn) =>
          turn.items
            .filter(
              (item, index) =>
                item.type === "userMessage" &&
                (item.id === p.clientUserMessageId ||
                  (index === 0 &&
                    turn.clientUserMessageId === p.clientUserMessageId)),
            )
            .map((item) => ({ turn, item })),
        )
      : [];
    if (
      matches.some(
        ({ item }) => JSON.stringify(item.content) !== JSON.stringify(p.input),
      )
    )
      throw new Error("This message identity has different content");
    const prior = matches.find(({ turn, item }) =>
      item.delivery === "steer"
        ? item.deliveryStatus !== "not_applied"
        : turn.startOutcome !== "not_applied",
    );
    if (prior) {
      const steered = prior.item.delivery === "steer";
      if (
        (steered ? prior.item.deliveryStatus : prior.turn.startOutcome) ===
        "preparing"
      )
        throw Object.assign(new Error("Claude input receipt outcome unknown"), {
          data: { turnStartOutcome: "unknown" },
        });
      return {
        turn: { id: prior.turn.id, status: prior.turn.status },
        ...(steered ? { steered: true } : {}),
      };
    }
    const running = queries.get(s.id)?.turn;
    if (running) {
      try {
        await handle({
          method: "turn/steer",
          params: {
            threadId: s.id,
            expectedTurnId: running.id,
            clientUserMessageId: p.clientUserMessageId ?? "",
            input: p.input,
          },
        });
        return {
          turn: { id: running.id, status: running.status },
          steered: true,
        };
      } catch (error) {
        // The running turn ended before this input reached Claude. Nothing
        // was submitted, so the input starts the next turn instead.
        if (!errorField(error, "steerNotSubmitted") || running.interrupted)
          throw error;
        await finishingTurns.get(running);
        if (queries.get(s.id)?.turn) throw error;
      }
    }
    let nativeCommand;
    if (p.claudeCommand) {
      const commands = await handle({
        method: "claude/commands",
        params: { cwd: s.cwd },
      });
      nativeCommand = explicitNativeCommand(p.claudeCommand, commands);
      // Unknown slash text remains ordinary user input; never extract commands from it.
    }
    const groups = await inputGroups(
      nativeCommand ? [{ type: "text", text: nativeCommand }] : p.input,
      content,
    );
    const turnId = randomUUID();
    const turn: Turn = {
      id: turnId,
      clientUserMessageId: p.clientUserMessageId,
      ...(p.idleCompaction ? { idleCompaction: true } : {}),
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
    const message = userMessage(
      s,
      turn,
      groups,
      p.clientUserMessageId || turn.id,
    );
    let active: ActiveQuery | null | undefined = queries.get(s.id);
    const startedAt = Date.now();
    const deadline = startedAt + PREPARATION_TIMEOUT_MS;
    const control = <T>(read: () => T | Promise<T>, phase: string) =>
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
            discardQuery(s.id, active);
        },
        phase,
        startedAt,
      );
    const requestedTools =
      exactTurnTools(s, p.dynamicTools) || s.dynamicTools || [];
    const toolsIdentity = JSON.stringify(requestedTools);
    const toolsChanged = active && active.toolsIdentity !== toolsIdentity;
    s.dynamicTools = requestedTools;
    if (active && !active.turn && !active.tasks.size) {
      // A persistent query can die between turns (its Claude process ends).
      // Nothing is running in it, so start this turn in a fresh query.
      try {
        await control(() => active?.ready, "active_ready");
      } catch (error) {
        if (!deadQuery(error)) throw error;
        discardQuery(s.id, active);
        active = null;
      }
    }
    if (active) {
      const live = active;
      await control(() => live.ready, "active_ready");
      const q = live.q;
      if (!q) throw new Error("Claude query is closed");
      if (toolsChanged) {
        // Replace only the Studio MCP server; background tasks keep running.
        // The SDK keeps an already registered in-process server even when its
        // tools change. Remove it first so the new schema replaces it.
        await control(
          () => q.setMcpServers(live.externalMcpServers || {}),
          "mcp_remove",
        );
        await control(
          () =>
            q.setMcpServers({
              ...live.externalMcpServers,
              studio: studioTools(s, () => live.turn),
            }),
          "mcp_install",
        );
        active.toolsIdentity = toolsIdentity;
      }
      try {
        await control(() => q.setModel(p.model || s.model), "model_set");
        await control(
          () => q.setPermissionMode(permissionMode(s, p)),
          "permissions_set",
        );
        active.permissionMode = permissionMode(s, p);
        const flagSettings = await control(
          () => flags(s, p, true),
          "catalog_flags",
        );
        await control(() => q.applyFlagSettings(flagSettings), "flag_settings");
      } catch (error) {
        if (!deadQuery(error) || active.turn || active.tasks.size) throw error;
        discardQuery(s.id, active);
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
      active.finalization = null;
      active.idleRequestStartedAt = Date.now();
      active.reservingInput = true;
    }
    s.model = p.model || s.model;
    if (!s.moveProof)
      s.moveTurnOptions = Object.fromEntries(
        (["effort", "serviceTier"] as const)
          .filter((key) => p[key] !== undefined)
          .map((key) => [key, p[key]]),
      );
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
      // No SDK admission occurred; retire only this reservation's guard and
      // save the cleanup (or defer it on a full disk) before reporting refusal.
      if (cancelNativeSessionInput(s, nativeInputAttempt(turn, turn.items[0])))
        await persistLive(s);
      throw isDiskFull(error) ? diskFullInputError(error) : error;
    }
    if (active) {
      active.reservingInput = false;
      if (active.turn !== turn || active.input.closed) {
        const error = new Error("Claude stopped before accepting the new turn");
        turn.status = "failed";
        turn.startOutcome = "not_applied";
        turn.error = { message: error.message };
        cancelNativeSessionInput(s, nativeInputAttempt(turn, turn.items[0]));
        pendingTurnReceipts.delete(turn.id);
        await persistLive(s);
        throw Object.assign(error, {
          data: { turnStartOutcome: "not_applied" },
        });
      }
    } else {
      active = newActive(turn);
      queries.set(s.id, active);
      active.consumer = new Promise((resolve, reject) => {
        const owned = active;
        if (!owned) return reject(new Error("Claude query is missing"));
        setImmediate(() => startSession(s, owned, p).then(resolve, reject));
      });
      active.consumer.catch((error) =>
        process.stderr.write("Claude consumer: " + errorText(error) + "\n"),
      );
    }
    catalogCache.activityChanged();
    emit("turn/started", {
      threadId: s.id,
      turn: { id: turn.id, status: "inProgress" },
    });
    active.input.push(message);
    turn.startOutcome = "accepted";
    // The reply reports the turn as Claude accepted it. The SDK stream runs
    // while the receipt saves, so a fast turn may already have ended; its
    // end arrives as turn/completed, as with Codex.
    const acceptedStatus = turn.status;
    try {
      // Claude has this input. A full disk only delays its saved receipt.
      await persistLive(s, turn);
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
    return { turn: { id: turn.id, status: acceptedStatus } };
  }
  if (method === "turn/steer") {
    const s = await session(p.threadId),
      active = queries.get(s.id);
    if (
      !active?.turn ||
      active.turn.interrupted ||
      active.turn.id !== p.expectedTurnId
    )
      throw new Error("The steer belongs to a different Claude turn");
    const turn = active.turn,
      id = p.clientUserMessageId;
    if (!id) throw new Error("Steer requires its original message identity");
    const previous = turn.items.find((item) => item.id === id);
    if (previous) {
      if (
        ["not_applied", "cancelled"].some(
          (status) => status === previous.deliveryStatus,
        )
      )
        throw new Error(previous.error);
      if (JSON.stringify(previous.content) !== JSON.stringify(p.input))
        throw new Error("This message identity has different content");
      return { turnId: turn.id };
    }
    // Errors with steerNotSubmitted: the input was never handed to Claude.
    const notSubmitted = (message: string) =>
      Object.assign(new Error(message), { steerNotSubmitted: true });
    if (finishingTurns.has(turn))
      throw notSubmitted(
        "Claude finished before this steer could be submitted",
      );
    active.pendingSteers.add(id);
    let item: Item | undefined;
    try {
      const groups = await inputGroups(p.input, content);
      if (
        turn.interrupted ||
        active.turn !== turn ||
        finishingTurns.has(turn) ||
        queries.get(s.id) !== active ||
        active.input.closed
      )
        throw notSubmitted(
          "Claude finished before this steer could be submitted",
        );
      item = {
        id,
        type: "userMessage",
        content: p.input,
        nativeId: nativeUserMessageId(id),
        delivery: "steer",
        deliveryStatus: "preparing",
      };
      turn.items.push(item);
      const message = userMessage(s, turn, groups, id);
      try {
        await persist(s);
      } catch (error) {
        throw isDiskFull(error) ? diskFullInputError(error) : error;
      }
      if (
        turn.interrupted ||
        active.turn !== turn ||
        finishingTurns.has(turn) ||
        queries.get(s.id) !== active ||
        active.input.closed
      )
        throw notSubmitted(
          "Claude stopped before this steer could be submitted",
        );
      active.input.push(message);
      item.deliveryStatus = "submitted";
      return { turnId: turn.id };
    } catch (error) {
      active.pendingSteers.delete(id);
      if (item) {
        item.deliveryStatus = "not_applied";
        cancelNativeSessionInput(s, nativeInputAttempt(turn, item));
        item.error = errorText(error);
        await persistLive(s);
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
    const s = await session(p.threadId);
    const active = queries.get(p.threadId);
    // Match Codex: an interrupt without an active turn changes nothing.
    // Closing an idle session here would also end its background tasks.
    if (!active?.turn) throw new Error("no active turn to interrupt");
    if (p.turnId && p.turnId !== active.turn.id)
      throw new Error("The interrupt belongs to a different Claude turn");
    const turn = active.turn;
    turn.interrupted = true;
    const reservingInput = active.reservingInput;
    if (reservingInput) {
      // An interrupt can race the durable pre-input receipt. Invalidate only
      // that reservation so the queued background work and query stay open.
      active.turn = null;
      active.reservingInput = false;
      catalogCache.activityChanged();
    }
    const stopTasks = p.stopBackgroundTasks !== false && active.tasks.size;
    if (stopTasks && typeof active.q?.stopTask !== "function")
      throw new Error("Claude does not support stopping background tasks");
    let results: PromiseSettledResult<void>[] = [];
    if (!reservingInput && !finishingTurns.has(turn) && !active.stopTimer) {
      const timer = setTimeout(() => {
        if (active.stopTimer !== timer) return;
        active.stopTimer = null;
        if (active.turn !== turn) return;
        void recoverStall(s, active, {
          id: "stop:" + turn.id,
          stop: true,
        }).catch((error) => lifecycleFailure(s, active, error));
      }, limits.interruptWaitMs);
      active.stopTimer = timer;
      timer.unref();
    }
    const receipt = await interruptInputQueue(
      s,
      active,
      stopTasks
        ? async () => {
            results = await Promise.allSettled(
              [...active.tasks.keys()].map((taskId) =>
                active.q?.stopTask(taskId),
              ),
            );
          }
        : undefined,
    );
    // The pending start owns the reservation's durable rejection. Do not
    // wait for its blocked save before the caller can observe the interrupt.
    if (!reservingInput) {
      if (active.turn === turn && active.deferredResult)
        await finishTurn(s, active, active.deferredResult);
      else await persistLive(s);
    }
    const failure = results.find((result) => result.status === "rejected");
    if (failure?.status === "rejected")
      throw Object.assign(
        new Error(
          failure.reason?.message || "Claude background task stop failed",
        ),
        {
          data: { interruptReceipt: receipt },
        },
      );
    return receipt;
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
  if (method === "thread/commandExecution/background")
    return moveCommandToBackground(queries.get(p.threadId), p);
  if (method === "claude/stopTask") {
    const active = queries.get(p.threadId);
    if (!active?.q || !active.tasks.has(p.taskId))
      throw new Error("This Claude background task is no longer active");
    await active.q.stopTask(p.taskId);
    return {};
  }
  if (method === "mcpServerStatus/list" || method === "mcpServer/reconnect") {
    await session(p.threadId);
    const active = queries.get(p.threadId);
    if (!active) throw new Error("The Claude session query is not loaded");
    await active.ready;
    if (queries.get(p.threadId) !== active || !active.q)
      throw new Error("The Claude session query is not loaded");
    if (method === "mcpServer/reconnect") {
      if (typeof p.name !== "string" || !p.name.trim())
        throw new Error("An MCP server name is required");
      if (typeof active.q.reconnectMcpServer !== "function")
        throw new Error("This Claude SDK does not support MCP reconnect");
      await active.q.reconnectMcpServer(p.name);
      return {};
    }
    const offset = p.cursor == null ? 0 : Number(p.cursor);
    if (!Number.isSafeInteger(offset) || offset < 0)
      throw new Error("Invalid Claude MCP status cursor");
    const limit = p.limit == null ? null : Number(p.limit);
    if (limit !== null && (!Number.isSafeInteger(limit) || limit < 1))
      throw new Error("Claude MCP status limit must be a positive integer");
    const statuses = await active.q.mcpServerStatus();
    const end = limit === null ? statuses.length : offset + limit;
    // SDK configuration can contain credentials. Discovery exposes status only.
    const data = statuses
      .slice(offset, end)
      .map(({ config: _config, ...status }) => status);
    return {
      data,
      nextCursor: end < statuses.length ? String(end) : null,
    };
  }
  if (method === "skills/list") return listSkills(p?.cwds);
  throw new Error("Claude does not support " + method);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function errorData(error: unknown): Record<string, unknown> {
  return isRecord(error) && isRecord(error.data) ? error.data : {};
}
function errorField(error: unknown, field: string): unknown {
  return isRecord(error) ? error[field] : undefined;
}
function previewInput(input: unknown): string | undefined {
  if (!Array.isArray(input)) return undefined;
  const block: unknown = input.find(
    (value: unknown) => isRecord(value) && value.type === "text",
  );
  return isRecord(block) && typeof block.text === "string"
    ? block.text.slice(0, 160)
    : undefined;
}
function errorText(error: unknown): string | undefined {
  const text = errorField(error, "message");
  return typeof text === "string" ? text : undefined;
}
function readBridgeInput<M extends BridgeServerMethod = BridgeServerMethod>(
  line: string,
): BridgeRequest | BridgeResponse<BridgeServerResult<M>> {
  const value: unknown = JSON.parse(line);
  if (!isRecord(value)) throw new Error("Invalid Claude bridge message");
  if ("method" in value && !isRecord(value.params)) value.params = {};
  // Studio and this bridge own the protocol; the envelope is checked here once.
  // Payload types come from the shared wire owner, as in the scripted child boundary.
  return value as BridgeRequest | BridgeResponse<BridgeServerResult<M>>;
}

const operations = new Map<string, Promise<unknown>>();
const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  let message;
  try {
    message = readBridgeInput(line);
  } catch {
    return;
  }
  if (!("method" in message)) {
    const waiting = pending.get(message.id);
    if (waiting) {
      pending.delete(message.id);
      if ("error" in message) waiting.reject(new Error(message.error.message));
      else waiting.resolve(line);
    }
    return;
  }
  const params = message.params;
  if (!isRecord(params)) return;
  // Serialize session mutations, but keep interrupt and permission replies independent.
  const key =
    "threadId" in params && typeof params.threadId === "string"
      ? params.threadId
      : undefined;
  const ordered = key && isThreadSerialized(message.method, params);
  const before = ordered
    ? operations.get(key) || Promise.resolve()
    : Promise.resolve();
  const operation = before.catch(() => {}).then(() => handle(message));
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
      if (message.id !== undefined)
        send({ id: message.id, result: result ?? null });
    },
    (error) => {
      if (message.id !== undefined)
        send({
          id: message.id,
          error: {
            code: Number.isInteger(errorField(error, "code"))
              ? errorField(error, "code")
              : -32000,
            message: errorText(error),
            ...(error.data === undefined ? {} : { data: error.data }),
          },
        });
    },
  );
});
let closing: Promise<never> | undefined;
function shutdown() {
  if (closing) return closing;
  armStallWatch.dispose();
  closing = (async () => {
    const activeQueries = [...queries.entries()];
    // Maximum graceful shutdown drain, in milliseconds; never wait for the
    // CLI process-exit budget when its SDK consumer cannot finish.
    const SHUTDOWN_DRAIN_LIMIT_MS = 2_000;
    const deadline =
      Date.now() + Math.min(limits.exitWaitMs, SHUTDOWN_DRAIN_LIMIT_MS);
    const bounded = async (operation: Promise<unknown> | undefined) => {
      let timer;
      try {
        await Promise.race([
          operation,
          new Promise((resolve) => {
            timer = setTimeout(resolve, Math.max(0, deadline - Date.now()));
          }),
        ]);
      } finally {
        clearTimeout(timer ?? undefined);
      }
    };
    for (const [, active] of activeQueries) {
      clearTurnTimers(active);
      active.input.close();
      closeQuery(active);
    }
    await bounded(
      Promise.allSettled(activeQueries.map(([, active]) => active.consumer)),
    );
    // If the SDK did not settle, final accounting remains explicitly unknown.
    for (const [threadId, active] of activeQueries)
      drainUsage(threadId, active);
    await bounded(commands.close());
    await bounded(sessionStore.drain());
    await bounded(
      (async () => {
        while (stdoutLines.length || stdoutDraining) {
          if (stdoutDraining)
            await new Promise((resolve) =>
              process.stdout.once("drain", resolve),
            );
          flushStdout();
        }
        await new Promise((resolve) => process.stdout.write("", resolve));
      })(),
    );
    process.exit(0);
  })();
  return closing;
}
lines.on("close", shutdown);
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
