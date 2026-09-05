#!/usr/bin/env node
// Start independent Codex workers from one JSON task file.
import { execFileSync, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface } from "node:readline";
import { expandHome, outsideClaude, codexHome } from "./codex-paths.mjs";
import {
  appendFileSync,
  closeSync,
  mkdirSync,
  openSync,
  readFileSync,
  realpathSync,
  readdirSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const SKILL_DIR = dirname(SCRIPT_DIR);
const CODEX_BIN = process.env.CODEX_BIN || "codex";
const MODEL = process.env.CODEX_MODEL || "gpt-5.6-luna";
const EFFORT = process.env.CODEX_EFFORT || "max";
const LOAD_LIMIT = Number(process.env.CODEX_LOAD_LIMIT || 20);
const CAPACITY_RETRY_MAX = Number(process.env.CODEX_CAPACITY_RETRIES || 6);
const CAPACITY_BACKOFF_MS = Number(process.env.CODEX_CAPACITY_BACKOFF_MS || 120_000);
const RPC_TIMEOUT_MS = Number(process.env.CODEX_RPC_TIMEOUT_MS || 30_000);
const INBOX_POLL_MS = Number(process.env.CODEX_INBOX_POLL_MS || 10_000);
const INBOX_RETRY_MAX = Number(process.env.CODEX_INBOX_RETRIES || 5);
const INBOX_BACKOFF_MS = Number(process.env.CODEX_INBOX_BACKOFF_MS || 5_000);
const WAVE = process.env.CODEX_WAVE || "main";
const RUN_ID = randomUUID();
const ORCHESTRATOR_ID = process.env.CODEX_ORCHESTRATOR_ID || process.env.CODEX_THREAD_ID || null;
const ORCHESTRATOR_NAME = process.env.CODEX_ORCHESTRATOR_NAME || "Orchestrator";
const APPROVAL_POLICY = process.env.CODEX_APPROVAL_POLICY || null;
const IMPLEMENTER_SANDBOX = process.env.CODEX_SANDBOX || null;
const ROLES = new Set(["implementer", "reviewer"]);
const APPROVAL_POLICIES = new Set(["untrusted", "on-request", "never"]);
const SANDBOXES = new Set(["read-only", "workspace-write", "danger-full-access"]);

if (!/^[A-Za-z0-9._-]+$/.test(WAVE)) {
  throw new Error("CODEX_WAVE must contain only letters, digits, dot, underscore, or hyphen");
}
if (!Number.isFinite(LOAD_LIMIT) || LOAD_LIMIT <= 0) {
  throw new Error("CODEX_LOAD_LIMIT must be a positive number");
}
if (!Number.isFinite(RPC_TIMEOUT_MS) || RPC_TIMEOUT_MS <= 0) {
  throw new Error("CODEX_RPC_TIMEOUT_MS must be a positive number");
}
if (!Number.isFinite(INBOX_POLL_MS) || INBOX_POLL_MS <= 0) {
  throw new Error("CODEX_INBOX_POLL_MS must be a positive number");
}
if (!Number.isInteger(INBOX_RETRY_MAX) || INBOX_RETRY_MAX <= 0) {
  throw new Error("CODEX_INBOX_RETRIES must be a positive integer");
}
if (!Number.isFinite(INBOX_BACKOFF_MS) || INBOX_BACKOFF_MS <= 0) {
  throw new Error("CODEX_INBOX_BACKOFF_MS must be a positive number");
}
if (!Number.isInteger(CAPACITY_RETRY_MAX) || CAPACITY_RETRY_MAX < 0) {
  throw new Error("CODEX_CAPACITY_RETRIES must be zero or a positive integer");
}
if (!Number.isFinite(CAPACITY_BACKOFF_MS) || CAPACITY_BACKOFF_MS <= 0) {
  throw new Error("CODEX_CAPACITY_BACKOFF_MS must be a positive number");
}
if (APPROVAL_POLICY && !APPROVAL_POLICIES.has(APPROVAL_POLICY)) {
  throw new Error("CODEX_APPROVAL_POLICY must be untrusted, on-request, or never");
}
if (IMPLEMENTER_SANDBOX && !SANDBOXES.has(IMPLEMENTER_SANDBOX)) {
  throw new Error("CODEX_SANDBOX must be read-only, workspace-write, or danger-full-access");
}

const stateBase = process.env.CODEX_AGENTS_STATE_DIR
  ? expandHome(process.env.CODEX_AGENTS_STATE_DIR)
  : join(
    process.env.XDG_STATE_HOME
      ? expandHome(process.env.XDG_STATE_HOME)
      : join(homedir(), ".local", "state"),
    "codex-agents",
  );
const statePath = resolve(stateBase);
outsideClaude(statePath);
const CODEX_PROFILE = codexHome();
mkdirSync(statePath, { recursive: true });
const STATE = realpathSync(statePath);

const TASKS_PATH = outsideClaude(
  process.env.CODEX_TASKS
    ? expandHome(process.env.CODEX_TASKS)
    : join(STATE, `codex-tasks.${WAVE}.json`),
);
const STATUS = join(STATE, `codex-swarm-status.${WAVE}.json`);
const READY = join(STATE, `codex-swarm-ready.${WAVE}.json`);
const EVENTS = join(STATE, `codex-swarm-events.${WAVE}.${RUN_ID}.jsonl`);
const STDERR = join(STATE, `codex-swarm-stderr.${WAVE}.${RUN_ID}.log`);
const LAUNCHER_PID_FILE = join(STATE, `codex-swarm-launcher.${WAVE}.pid`);
const INBOX = join(STATE, `codex-inbox.${WAVE}.${RUN_ID}`);
const DEAD_LETTER = join(STATE, `codex-dead-letter.${WAVE}.${RUN_ID}`);
const BOARD_STATE = outsideClaude(process.env.CODEX_BOARD_STATE_DIR
  ? expandHome(process.env.CODEX_BOARD_STATE_DIR) : join(STATE, "board"));
const WORKTREE_LOCK_DIR = join(STATE, "worktree-locks");
const BOARD_COMMAND = resolve(join(SCRIPT_DIR, "codex-board"));

const budgetText = process.env.CODEX_BUDGET;
const BUDGET = budgetText === undefined || budgetText === "" ? null : Number(budgetText);
if (BUDGET !== null && (!Number.isInteger(BUDGET) || BUDGET <= 0)) {
  throw new Error("CODEX_BUDGET must be a positive integer");
}

function readTasks() {
  let value;
  try {
    value = JSON.parse(readFileSync(TASKS_PATH, "utf8"));
  } catch (error) {
    throw new Error(`Cannot read task file ${TASKS_PATH}: ${error.message}`);
  }
  if (!Array.isArray(value) || value.length === 0) {
    throw new Error("The task file must contain a non-empty JSON array");
  }
  const names = new Set();
  const worktrees = new Set();
  for (const [index, task] of value.entries()) {
    for (const field of ["name", "cwd", "branch", "objective", "prompt"]) {
      if (typeof task?.[field] !== "string" || !task[field].trim()) {
        throw new Error(`Task ${index} needs a non-empty ${field} string`);
      }
    }
    if (!/^[A-Za-z0-9._-]+$/.test(task.name)) {
      throw new Error(`Task name ${task.name} contains an unsupported character`);
    }
    if (names.has(task.name)) throw new Error(`Duplicate task name: ${task.name}`);
    names.add(task.name);
    task.role = task.role || "implementer";
    const parent = task.orchestratorId ?? ORCHESTRATOR_ID;
    if (parent !== null && (typeof parent !== 'string' || !/^[A-Za-z0-9._:/-]{1,200}$/.test(parent))) {
      throw new Error(`Task ${task.name} has an invalid orchestrator identity`);
    }
    task.orchestratorId = parent;
    if (!ROLES.has(task.role)) {
      throw new Error(`Task ${task.name} role must be implementer or reviewer`);
    }
    if (!task.cwd.startsWith("/")) throw new Error(`Task ${task.name} needs an absolute cwd`);
    validateWorktree(task);
    if (worktrees.has(task.cwd)) throw new Error(`Duplicate worktree path: ${task.cwd}`);
    worktrees.add(task.cwd);
  }
  return value;
}

function gitOutput(cwd, args) {
  return execFileSync("git", ["-C", cwd, ...args], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function validateWorktree(task) {
  try {
    task.cwd = realpathSync(task.cwd);
    const root = realpathSync(gitOutput(task.cwd, ["rev-parse", "--show-toplevel"]));
    if (root !== task.cwd) {
      throw new Error(`cwd must be the worktree root ${root}`);
    }

    const branch = gitOutput(task.cwd, ["symbolic-ref", "--quiet", "--short", "HEAD"]);
    if (branch !== task.branch) {
      throw new Error(`branch is ${branch}, task says ${task.branch}`);
    }
    if (branch === "main" || branch === "master") {
      throw new Error(`branch ${branch} is not a worker branch`);
    }

    const gitDir = realpathSync(gitOutput(task.cwd, ["rev-parse", "--absolute-git-dir"]));
    const commonDir = realpathSync(
      resolve(task.cwd, gitOutput(task.cwd, ["rev-parse", "--git-common-dir"])),
    );
    if (gitDir === commonDir) {
      throw new Error("cwd is the primary checkout, not a linked worktree");
    }
  } catch (error) {
    throw new Error(`Task ${task.name} has an invalid worktree: ${error.message}`);
  }
}

const tasks = readTasks();
const preamblePath = resolve(
  process.env.CODEX_PREAMBLE
    ? expandHome(process.env.CODEX_PREAMBLE)
    : join(SKILL_DIR, "prompts", "worker-preamble.md"),
);
const preambleTemplate = readFileSync(preamblePath, "utf8");
const shellLiteral = (value) => `'${String(value).replaceAll("'", `'"'"'`)}'`;
const roleRules = (task) => task.role === "reviewer"
  ? "Do not edit files. Do not commit. Review the assigned evidence and report exact findings."
  : "Edit only the assigned worktree. Commit each finished part. Stage files by name. Do not use git commit -a.";
const verificationRules = (task) => task.role === "reviewer"
  ? "Inspect the supplied diff and evidence. Run only read-only checks. Do not run a check that writes build files."
  : "Run the affected tests. Include one test for the new behavior and one regression test when applicable. Commit before the final report.";
const workerPrompt = (task, boardOwner) => preambleTemplate
  .replaceAll("{{STATE_DIR}}", shellLiteral(STATE))
  .replaceAll("{{BOARD_STATE_DIR}}", shellLiteral(BOARD_STATE))
  .replaceAll("{{BOARD_COMMAND}}", shellLiteral(BOARD_COMMAND))
  .replaceAll("{{WORKER_NAME}}", shellLiteral(task.name))
  .replaceAll("{{BOARD_OWNER}}", shellLiteral(boardOwner))
  .replaceAll("{{LOAD_LIMIT}}", String(LOAD_LIMIT))
  .replaceAll("{{ROLE_RULES}}", roleRules(task))
  .replaceAll("{{VERIFICATION_RULES}}", verificationRules(task))
  + "\n\n---\n\n" + task.prompt;

function processIsAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

const heldWorktreeLocks = [];

function worktreeLockPath(cwd) {
  const key = createHash("sha256").update(cwd).digest("hex");
  return join(WORKTREE_LOCK_DIR, `${key}.json`);
}

function acquireWorktreeLocks() {
  mkdirSync(WORKTREE_LOCK_DIR, { recursive: true });
  for (const task of [...tasks].sort((left, right) => left.cwd.localeCompare(right.cwd))) {
    const path = worktreeLockPath(task.cwd);
    while (true) {
      try {
        const handle = openSync(path, "wx");
        try {
          writeFileSync(handle, JSON.stringify({
            cwd: task.cwd,
            wave: WAVE,
            runId: RUN_ID,
            launcherPid: process.pid,
          }) + "\n");
        } finally {
          closeSync(handle);
        }
        heldWorktreeLocks.push(path);
        break;
      } catch (error) {
        if (error.code !== "EEXIST") throw error;
        let owner;
        try {
          owner = JSON.parse(readFileSync(path, "utf8"));
        } catch (readError) {
          throw new Error(`Cannot validate worktree lock ${path}: ${readError.message}`);
        }
        if (
          owner?.cwd !== task.cwd
          || typeof owner?.launcherPid !== "number"
          || typeof owner?.wave !== "string"
          || typeof owner?.runId !== "string"
        ) {
          throw new Error(`Invalid worktree lock ${path}`);
        }
        if (processIsAlive(owner.launcherPid)) {
          throw new Error(
            `worktree ${task.cwd} is active in wave ${owner.wave} launcher ${owner.launcherPid}`,
          );
        }
        try {
          unlinkSync(path);
        } catch (unlinkError) {
          if (unlinkError.code !== "ENOENT") throw unlinkError;
        }
      }
    }
  }
}

function releaseWorktreeLocks() {
  for (const path of heldWorktreeLocks) {
    try {
      const owner = JSON.parse(readFileSync(path, "utf8"));
      if (owner.runId === RUN_ID && owner.launcherPid === process.pid) unlinkSync(path);
    } catch {
      // The lock is absent or does not belong to this launcher.
    }
  }
}

function acquireLauncherPid() {
  while (true) {
    try {
      const handle = openSync(LAUNCHER_PID_FILE, "wx");
      try {
        writeFileSync(handle, String(process.pid));
      } finally {
        closeSync(handle);
      }
      return;
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
      let priorPid = 0;
      try {
        priorPid = Number(readFileSync(LAUNCHER_PID_FILE, "utf8").trim());
      } catch {
        priorPid = 0;
      }
      if (
        priorPid
        && priorPid !== process.pid
        && processIsAlive(priorPid)
      ) {
        console.error(`CODEX_SWARM_REFUSED wave=${WAVE} launcher=${priorPid} is active`);
        process.exit(2);
      }
      try {
        unlinkSync(LAUNCHER_PID_FILE);
      } catch (unlinkError) {
        if (unlinkError.code !== "ENOENT") throw unlinkError;
      }
    }
  }
}

acquireLauncherPid();
try {
  unlinkSync(READY);
} catch (error) {
  if (error.code !== "ENOENT") throw error;
}
mkdirSync(INBOX, { recursive: true });
mkdirSync(DEAD_LETTER, { recursive: true });
mkdirSync(BOARD_STATE, { recursive: true });

let proc;
let shuttingDown = false;
const byThread = new Map();
function cleanupLauncher() {
  releaseWorktreeLocks();
  try {
    if (Number(readFileSync(LAUNCHER_PID_FILE, "utf8").trim()) === process.pid) {
      unlinkSync(LAUNCHER_PID_FILE);
    }
  } catch {
    // The file is already absent or belongs to a replacement launcher.
  }
}
process.on("exit", cleanupLauncher);
try {
  acquireWorktreeLocks();
} catch (error) {
  console.error(`CODEX_SWARM_REFUSED ${error.message}`);
  process.exit(2);
}
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    shuttingDown = true;
    for (const state of byThread.values()) {
      if (["starting", "running", "waiting", "capacity-retry", "paused"].includes(state.turnStatus)) {
        state.turnStatus = "interrupted";
        state.error = `launcher stopped by ${signal}`;
      }
    }
    flush();
    proc?.kill(signal);
    process.exit(signal === "SIGINT" ? 130 : 143);
  });
}

const appServerArgs = ["app-server", "--listen", "stdio://"];
if (process.env.CODEX_SERVICE_TIER) {
  appServerArgs.push("-c", `service_tier="${process.env.CODEX_SERVICE_TIER}"`);
}
proc = spawn(CODEX_BIN, appServerArgs, {
  env: { ...process.env, CODEX_HOME: CODEX_PROFILE },
  stdio: ["pipe", "pipe", "pipe"],
});

function abortLauncher(error) {
  if (shuttingDown) return;
  shuttingDown = true;
  const message = error?.message || String(error);
  for (const state of byThread.values()) {
    if (["starting", "running", "waiting", "capacity-retry"].includes(state.turnStatus)) {
      state.turnStatus = "failed";
      state.error = message;
    }
  }
  flush();
  console.error(`CODEX_SWARM_FAILED ${message}`);
  proc?.kill("SIGTERM");
  process.exit(1);
}

process.on("uncaughtException", abortLauncher);
process.on("unhandledRejection", abortLauncher);

let nextId = 1;
const pending = new Map();

function call(method, params = {}) {
  const id = nextId++;
  proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  return new Promise((resolveCall, rejectCall) => {
    const timeout = setTimeout(() => {
      pending.delete(id);
      rejectCall(new Error(`${method} timed out after ${RPC_TIMEOUT_MS} ms`));
    }, RPC_TIMEOUT_MS);
    pending.set(id, {
      resolveCall: (value) => {
        clearTimeout(timeout);
        resolveCall(value);
      },
      rejectCall: (error) => {
        clearTimeout(timeout);
        rejectCall(error);
      },
    });
  });
}

function publicState(state) {
  const { prompt, currentInput, ...bounded } = state;
  return bounded;
}

function writeJsonAtomic(path, value) {
  const temporary = `${path}.${process.pid}.tmp`;
  writeFileSync(temporary, JSON.stringify(value, null, 1) + "\n");
  renameSync(temporary, path);
}

function flush() {
  writeJsonAtomic(STATUS, [...byThread.values()].map(publicState));
}

function appendEvent(value) {
  appendFileSync(EVENTS, JSON.stringify({ at: new Date().toISOString(), wave: WAVE, ...value }) + "\n");
}

function sandboxPolicy(mode, cwd) {
  if (mode === "read-only") return { type: "readOnly" };
  if (mode === "workspace-write") {
    return {
      type: "workspaceWrite",
      writableRoots: [cwd, BOARD_STATE],
      networkAccess: false,
      excludeSlashTmp: true,
      excludeTmpdirEnvVar: true,
    };
  }
  if (mode === "danger-full-access") return { type: "dangerFullAccess" };
  return null;
}

function threadPolicy(task) {
  const policy = {};
  if (APPROVAL_POLICY) policy.approvalPolicy = APPROVAL_POLICY;
  const sandbox = task.role === "reviewer" ? "read-only" : IMPLEMENTER_SANDBOX;
  if (sandbox) policy.sandbox = sandbox;
  return policy;
}

function turnPolicy(state) {
  const policy = {};
  if (APPROVAL_POLICY) policy.approvalPolicy = APPROVAL_POLICY;
  const mode = state.role === "reviewer" ? "read-only" : IMPLEMENTER_SANDBOX;
  const sandbox = sandboxPolicy(mode, state.cwd);
  if (sandbox) policy.sandboxPolicy = sandbox;
  return policy;
}

async function startTurn(state, text, messageId = randomUUID()) {
  state.currentInput = text;
  state.currentMessageId = messageId;
  const priorStatus = state.turnStatus;
  state.turnStatus = "starting";
  state.turnId = null;
  state.lastTurnStartedAt = new Date().toISOString();
  state.error = null;
  let serviceTier = process.env.CODEX_SERVICE_TIER || null;
  try {
    const tierFile = join(STATE, `service-tier.${WAVE}`);
    const v = readFileSync(tierFile, "utf8").trim();
    if (v) serviceTier = v;
  } catch {}
  let result;
  try {
    result = await call("turn/start", {
      threadId: state.threadId,
      model: state.requestedModel,
      effort: state.effort,
      ...(serviceTier ? { serviceTier } : {}),
      ...turnPolicy(state),
      cwd: state.cwd,
      runtimeWorkspaceRoots: [state.cwd],
      clientUserMessageId: messageId,
      input: [{ type: "text", text }],
    });
  } catch (error) {
    if (state.turnStatus === "starting") state.turnStatus = priorStatus;
    throw error;
  }
  // Notifications can arrive in the same chunk as the RPC response, before
  // this continuation resumes. Do not overwrite their terminal state or a
  // newer automatically started turn.
  state.turnId ??= result?.turn?.id ?? null;
  if (state.turnStatus === "starting") state.turnStatus = "running";
  flush();
}

async function steerTurn(state, text, messageId) {
  if (!state.turnId) throw new Error(`worker ${state.name} has no active turn id`);
  const result = await call("turn/steer", {
    threadId: state.threadId,
    expectedTurnId: state.turnId,
    clientUserMessageId: messageId,
    input: [{ type: "text", text }],
  });
  if (result?.turnId !== state.turnId) {
    throw new Error(`turn/steer returned unexpected turn id for ${state.name}`);
  }
  state.lastEvent = new Date().toISOString();
  flush();
}

async function deliverMessage(state, message) {
  if (state.turnStatus === "running") {
    await steerTurn(state, message.text, message.id);
    return "steered";
  }
  if (["budgetLimited", "usageLimited"].includes(state.goalStatus)) {
    throw new Error(`worker ${state.name} stopped at goal ${state.goalStatus}`);
  }
  if (!["waiting", "completed", "failed", "interrupted", "blocked", "paused"].includes(state.turnStatus)) {
    throw new Error(`worker ${state.name} cannot accept a message while ${state.turnStatus}`);
  }
  if (["complete", "blocked", "paused"].includes(state.goalStatus)) {
    await call("thread/goal/set", { threadId: state.threadId, status: "active" });
    state.goalStatus = "active";
  }
  await startTurn(state, message.text, message.id);
  return "started";
}

async function retryCapacity(state) {
  if (["budgetLimited", "usageLimited", "blocked", "paused"].includes(state.goalStatus)) return;
  try {
    await startTurn(state, state.currentInput, state.currentMessageId);
  } catch (error) {
    state.turnStatus = "failed";
    state.error = `capacity retry ${state.capacityRetries} failed: ${error?.message ?? error}`;
    flush();
  }
}

createInterface({ input: proc.stdout }).on("line", (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (message.id !== undefined && pending.has(message.id)) {
    const request = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) {
      request.rejectCall(new Error(message.error.message || JSON.stringify(message.error)));
    } else {
      request.resolveCall(message.result);
    }
    return;
  }

  const threadId = message.params?.threadId ?? message.params?.thread?.id;
  const state = threadId && byThread.get(threadId);
  if (!state) return;

  state.events += 1;
  state.lastEvent = new Date().toISOString();
  if (message.method === "item/agentMessage/delta" && message.params?.delta) {
    state.tail = (state.tail + message.params.delta).slice(-300);
  }
  if (message.method === "turn/started") {
    const turn = message.params?.turn || {};
    if (typeof turn.id === "string" && turn.id) state.turnId = turn.id;
    state.lastTurnStatus = turn.status ?? "inProgress";
    if (!["budgetLimited", "usageLimited", "blocked", "paused"].includes(state.goalStatus)) {
      state.turnStatus = "running";
      state.error = null;
    }
  }
  if (message.method === "thread/goal/updated") {
    const status = message.params?.goal?.status;
    state.goalStatus = status ?? state.goalStatus;
    state.tokensUsed = message.params?.goal?.tokensUsed ?? state.tokensUsed;
    if (status === "budgetLimited" || status === "usageLimited") {
      state.turnStatus = "failed";
      state.error = `goal ${status} at ${state.tokensUsed} tokens`;
    } else if (status === "blocked") {
      state.turnStatus = "blocked";
      state.error = "goal blocked";
    } else if (status === "paused") {
      state.turnStatus = "paused";
      state.error = "goal paused";
    } else if (status === "complete") {
      if (["waiting", "completed"].includes(state.turnStatus)) {
        state.turnStatus = "completed";
        state.error = null;
      }
    }
  }
  if (message.method === "turn/completed") {
    const turn = message.params?.turn || {};
    const error = turn.error?.message ?? null;
    state.lastTurnStatus = turn.status ?? "missing";
    const goalStopsTurn = ["budgetLimited", "usageLimited", "blocked", "paused"].includes(
      state.goalStatus,
    );
    if (!goalStopsTurn) {
      if (turn.status === "completed" && !error) {
        state.turnStatus = state.goalStatus === "complete" ? "completed" : "waiting";
        state.error = null;
      } else if (turn.status === "interrupted") {
        state.turnStatus = "interrupted";
        state.error = error || "turn interrupted";
      } else {
        state.turnStatus = "failed";
        state.error = error || `unexpected terminal turn status: ${turn.status ?? "missing"}`;
      }
    }
    if (
      !goalStopsTurn
      && /at capacity/i.test(error ?? "")
      && state.capacityRetries < CAPACITY_RETRY_MAX
    ) {
      state.capacityRetries += 1;
      state.turnStatus = "capacity-retry";
      state.error = error;
      const waitMs = CAPACITY_BACKOFF_MS * state.capacityRetries;
      appendEvent({ name: state.name, capacityRetry: state.capacityRetries, waitMs });
      setTimeout(() => void retryCapacity(state), waitMs);
    }
  }
  if (message.method && !message.method.includes("Delta")) {
    appendEvent({ name: state.name, method: message.method });
  }
  flush();
});

proc.stderr.on("data", (data) => appendFileSync(STDERR, data.toString()));
proc.on("error", abortLauncher);
proc.on("exit", (code, signal) => {
  if (shuttingDown) return;
  const detail = signal ? `signal ${signal}` : `code ${code}`;
  const error = new Error(`codex app-server stopped with ${detail}`);
  for (const request of pending.values()) request.rejectCall(error);
  pending.clear();
  for (const state of byThread.values()) {
    if (["starting", "running", "waiting", "capacity-retry"].includes(state.turnStatus)) {
      state.turnStatus = "failed";
      state.error = error.message;
    }
  }
  flush();
  console.error(`CODEX_SWARM_FAILED ${error.message}`);
  process.exit(code || 1);
});

await call("initialize", {
  clientInfo: { name: "codex-agents", version: "1" },
  capabilities: { experimentalApi: true },
});

for (const task of tasks) {
  const boardOwner = `${WAVE}:${RUN_ID}:${task.name}`;
  const started = await call("thread/start", {
    cwd: task.cwd,
    model: MODEL,
    ...threadPolicy(task),
    runtimeWorkspaceRoots: [task.cwd],
    threadSource: "subagent",
    developerInstructions: task.role === "reviewer"
      ? `You are a read-only Codex reviewer. Work only in ${task.cwd}. Do not edit or commit.`
      : `You are a Codex implementer. Work only in ${task.cwd}. Commit on ${task.branch}. Do not push.`,
  });
  const threadId = started?.thread?.id;
  if (!threadId) throw new Error(`thread/start returned no thread id for ${task.name}`);
  const prompt = workerPrompt(task, boardOwner);

  const state = {
    wave: WAVE,
    runId: RUN_ID,
    launcherPid: process.pid,
    name: task.name,
    role: task.role,
    orchestratorId: task.orchestratorId,
    orchestratorName: ORCHESTRATOR_NAME,
    boardOwner,
    threadId,
    turnId: null,
    branch: task.branch,
    cwd: task.cwd,
    requestedModel: MODEL,
    model: started?.model ?? null,
    effort: EFFORT,
    events: 0,
    lastEvent: null,
    turnStatus: "starting",
    goalStatus: null,
    tokensUsed: 0,
    error: null,
    tail: "",
    capacityRetries: 0,
    lastTurnStatus: null,
    currentInput: null,
    currentMessageId: null,
    mailboxError: null,
    startedAt: new Date().toISOString(),
    lastTurnStartedAt: null,
    prompt,
  };
  byThread.set(threadId, state);
  flush();

  const goalParams = { threadId, objective: task.objective, status: "active" };
  if (BUDGET !== null) goalParams.tokenBudget = BUDGET;
  await call("thread/goal/set", goalParams);
  state.goalStatus = "active";
  await startTurn(state, prompt);
}

writeJsonAtomic(READY, {
  wave: WAVE,
  runId: RUN_ID,
  launcherPid: process.pid,
  readyAt: new Date().toISOString(),
  threadIds: [...byThread.keys()],
});

const inboxBusy = new Set();
setInterval(async () => {
  for (const file of readdirSync(INBOX)) {
    if (!file.endsWith(".json")) continue;
    const name = file.slice(0, -5);
    if (inboxBusy.has(name)) continue;
    const state = [...byThread.values()].find((item) => item.name === name);
    if (!state) continue;
    const path = join(INBOX, file);
    let message;
    try {
      message = JSON.parse(readFileSync(path, "utf8"));
      if (
        typeof message?.id !== "string"
        || !message.id
        || typeof message?.text !== "string"
        || !message.text.trim()
        || !Number.isInteger(message?.attempts)
        || message.attempts < 0
        || typeof message?.nextAttemptAt !== "number"
      ) {
        throw new Error("invalid mailbox message");
      }
    } catch (error) {
      const destination = join(DEAD_LETTER, `${file}.${Date.now()}.invalid`);
      renameSync(path, destination);
      state.mailboxError = error.message;
      appendEvent({ name, mailboxDeadLetter: destination, error: error.message });
      flush();
      continue;
    }
    if (message.nextAttemptAt > Date.now()) continue;
    inboxBusy.add(name);
    try {
      const action = await deliverMessage(state, message);
      unlinkSync(path);
      state.mailboxError = null;
      appendEvent({ name, mailbox: action, messageId: message.id, chars: message.text.length });
      flush();
    } catch (error) {
      const errorMessage = error?.message ?? String(error);
      message.attempts += 1;
      message.lastError = errorMessage;
      state.mailboxError = errorMessage;
      if (message.attempts >= INBOX_RETRY_MAX) {
        const destination = join(DEAD_LETTER, `${name}.${message.id}.json`);
        writeJsonAtomic(path, message);
        renameSync(path, destination);
        appendEvent({
          name,
          mailboxDeadLetter: destination,
          messageId: message.id,
          attempts: message.attempts,
          error: errorMessage,
        });
      } else {
        const waitMs = INBOX_BACKOFF_MS * (2 ** (message.attempts - 1));
        message.nextAttemptAt = Date.now() + waitMs;
        writeJsonAtomic(path, message);
        appendEvent({
          name,
          mailboxRetry: message.attempts,
          messageId: message.id,
          waitMs,
          error: errorMessage,
        });
      }
      flush();
    } finally {
      inboxBusy.delete(name);
    }
  }
}, INBOX_POLL_MS);

console.log(JSON.stringify({
  wave: WAVE,
  runId: RUN_ID,
  stateDir: STATE,
  workers: [...byThread.values()].map((state) => ({
    name: state.name,
    threadId: state.threadId,
    model: state.model,
  })),
}));
