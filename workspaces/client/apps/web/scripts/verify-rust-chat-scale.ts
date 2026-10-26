#!/usr/bin/env node
// Explicitly selected live provider load verifier. Never run from the ordinary test tiers.
import assert from "node:assert/strict";
import type { components } from "../src/generated/api.ts";
import type { ChildProcess } from "node:child_process";
import type { ScaleVerifierLifecycle } from "./scaleVerifierTeardown.ts";
import type {
  ScaleProfile,
  ServedModelRecord,
  RetainedInput,
  EntityPage,
  StateFrame,
  Barrier,
} from "./scaleVerifierHelpers.ts";
import { errorCode } from "./scaleVerifierHelpers.ts";
import { runScaleVerifier } from "./scaleVerifierTeardown.ts";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { randomUUID } from "node:crypto";
import {
  mkdir,
  lstat,
  readFile,
  readdir,
  realpath,
  rename,
  rmdir,
  statfs,
  unlink,
  writeFile,
} from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  SCALE_WORKSPACE_MODE,
  TOOL_BARRIER_KILL_GRACE_SECONDS,
  TOOL_BARRIER_MAX_SECONDS,
  acquireRunLock,
  assertConcurrentWorkers,
  assertOwnedEntry,
  assertStoredScaleProfile,
  callbackProcessRetirementDelayMs,
  callbackWorktreeCwd,
  canonicalJournalBootId,
  classifyRetainedInputs,
  classifyStageQueues,
  createBoundedEventQueue,
  createReadinessMarkerDirectory,
  createStateResourceAdapter,
  findFifoHolders,
  finishedWorkspaceCleanupIds,
  foreignLiveTeamRows,
  forEachBounded,
  normalizedWorktreePath,
  parseProviderReplyJournalMessage,
  parseProviderStdinJournalMessage,
  parseWorkerWorkspace,
  readinessMarkerName,
  releaseFifoBarriers,
  requestIdentityKey,
  requireScaleModel,
  resumeIntentState,
  RETAINED_INPUT_STATUSES,
  THREAD_REQUEST_METHODS,
  scaleWorkerDefaults,
  sharedBarrierPaths,
  stateResourceVersion,
  watchReadinessMarkers,
  waitForServedModelEvidence,
  scaleProfile,
  scaleAccountKeys,
  releaseBarriersNow,
  readProcessTable,
  sampleProviderProcesses,
  showsUpstreamLimit,
} from "./scaleVerifierHelpers.ts";

type StdinEntry = NonNullable<
  ReturnType<typeof parseProviderStdinJournalMessage>
>;
type ReplyEntry = NonNullable<
  ReturnType<typeof parseProviderReplyJournalMessage>
>;
type AttemptEvidence = Pick<
  Schema["ExecutionAttempt"],
  "id" | "accountKey" | "events" | "requestTicket" | "connectionGeneration"
> & { runAccountKey?: Schema["ExecutionAttempt"]["accountKey"] };
export type RetainedInputRows = RetainedInput[];
type TeamChild = {
  id: string;
  initialInputNotSent: boolean;
  workspaceCleanupIntent: unknown;
} & {
  [
    K in
      | "deletedAt"
      | "cwd"
      | "branch"
      | "workspaceMode"
      | "worktreeReady"
      | "cleanedWorktree"
      | "rootId"
      | "parentId"
  ]: Schema["AgentRecordResponse"][K] | null;
};
type Schema = components["schemas"];
type FilesystemSnapshot = Awaited<ReturnType<typeof readFilesystemSnapshot>>;
type Client = Awaited<ReturnType<typeof createClient>>;
type SyncStream = Awaited<ReturnType<typeof startSyncResourceStream>>;
type ReadinessWatch = Awaited<ReturnType<typeof watchReadinessMarkers>>;
type BackendIdentity = {
  pid: number;
  stateDir: string;
  backendKind: string | null;
  backendBuild: Schema["DesktopResponse"]["backendBuild"] | null;
  bootId?: string;
};
type Intent = {
  state: string;
  error?: string;
  preparedAt?: string;
  responseAt?: string;
  elapsedMs?: number;
  receipt?: string | null;
  status?: string | null;
  targetId?: string;
  descendants?: boolean;
  requestId?: Schema["CreateAgentRequest"]["id"];
  method?: string;
  path?: string;
  finishedAt?: string;
  resolvedAt?: string;
  args?: string[];
  inspectedAbsent?: boolean;
};
type BarrierRecord = {
  barrierDirectory: string;
  barrierPath: string;
  readinessMarkerPath?: string;
  readinessObservedAt?: string;
  completionTag?: string;
};
type Worktree = Partial<BarrierRecord> & {
  cwd: string;
  branch?: string | null | undefined;
  cleanup: string;
  registeredRoot?: string;
  removeIntent?: Intent;
  branchDeleteIntent?: Intent;
};
type OwnedAgent = {
  id: string;
  parentId: string | null;
  isLead: boolean;
  stageWorkers: number;
  stageIndex?: number;
  create: Intent;
  exchanges: Intent[];
  cleanup: Intent & {
    at?: string;
    stop?: Intent;
    stopCommand?: string | null;
    stopElapsedMs?: number | null;
    deleteCommand?: string;
  };
  worktree?: Worktree;
  sharedBarrier?: BarrierRecord;
  callbackReadinessObservedAt?: string;
  callbackReleasedAt?: string;
};
type HttpRecord = {
  at: string;
  method: string;
  path: string;
  status: number | null;
  elapsedMs: number;
  outcome?: string;
  error?: string;
  eventStream?: boolean;
};
type Cleanup = Intent & {
  agents?: unknown[];
  backendIdentity?: Awaited<ReturnType<typeof verifyBackendIdentity>>;
  startedAt?: string;
  resumedAt?: string;
  filesystemDisk?: {
    at: string;
    availableBytesBefore: number;
    availableBytesAfter: number;
    usedDeltaBytes: number;
    deltaAttribution: string;
  };
  evidenceError?: string;
  retainedInputs?: ReturnType<typeof verifyRetainedInputsAfterCleanup>;
};
type StageCleanup = Partial<Intent> & {
  delete?: Intent;
  foreignChildrenObserved?: string[];
  workerStopObservationError?: string;
  beforeMutation?: TeamAudit;
  afterCleanup?: TeamAudit;
};
type TeamAudit = {
  at: string;
  rowsObserved: number;
  ownedCurrentRows: number;
  foreignRows: number;
  source: string;
};
type DiagnosticSnapshot = Partial<
  Pick<
    Schema["DiagnosticsResponse"],
    | "httpRequestMetrics"
    | "runtimeLockOperations"
    | "sqliteContention"
    | "queues"
    | "processTree"
    | "blockingPoolMetrics"
  >
> & {
  at?: string;
  upstreamLimits?: string;
  unavailable?: string;
  error?: string;
};
type Stage = {
  workers: number;
  state: string;
  agentIds: string[];
  turnIntervals: {
    agentId: string;
    turnId: string;
    startedAtMs: number;
    observedAt: string;
    endedAtMs?: number;
  }[];
  maxConcurrentTurns: number;
  createLatenciesMs: number[];
  secondExchangeLatenciesMs: number[];
  leadId?: string;
  startedAt?: string;
  finishedAt?: string;
  backendPid?: number;
  readinessMarkerDirectory?: string;
  workerScriptDirectory?: string;
  sharedBarrierDirectory?: string;
  createStartedAt?: string;
  createsConfirmed?: number;
  leadExchangeCompleted?: boolean;
  leadExchangeError?: string;
  allWorkersActiveAt?: string;
  toolCallbacksReadyCount?: number;
  toolBarrierTokens?: string[];
  toolBarrierRelease?: Intent & { intentAt?: string; confirmedAt?: string };
  toolBarrierReleasePaths?: string[];
  toolBarrierWorkerDirectories?: string[];
  toolBarrierState?: string;
  barrierReleasedAt?: string;
  firstExchangeCount?: number;
  secondExchangeCount?: number;
  secondExchangeIds?: string[];
  secondExchangeResults?: {
    id: string | undefined;
    state: string;
    error: string | null;
  }[];
  createResults?: {
    id: string | undefined;
    state: string;
    error: string | null;
  }[];
  leadWakeSuppression?: Intent & {
    intentAt?: string;
    confirmedAt?: string;
    leadStatus?: Schema["AgentEntityDto"]["status"];
    autoWake?: boolean;
  };
  settledCount?: number;
  leadNativeTurnCount?: number;
  elapsedMs?: number;
  apiLatencyPass?: boolean;
  providerStdinLatencyPass?: boolean;
  stageCleanupStartedAt?: string;
  stageCleanupFinishedAt?: string;
  cleanup?: StageCleanup;
  nativeTurnCounts?: Record<string, number>;
  diagnosticsBefore?: DiagnosticSnapshot;
  diagnosticsAfter?: DiagnosticSnapshot;
  leadServedModel?: Awaited<ReturnType<typeof verifyServedModel>>;
  firstExchangeServedModel?: Awaited<ReturnType<typeof verifyServedModel>>;
  secondExchangeServedModel?: Awaited<ReturnType<typeof verifyServedModel>>;
  concurrentWorkers?: ReturnType<typeof assertConcurrentWorkers>;
  providerProcessesAtPeak?: ReturnType<typeof sampleProviderProcesses>;
  worktreeDisk?: Awaited<ReturnType<typeof verifyOwnedWorktreeDisk>>;
  providerStdinJournalWindow?: { startedAt: string; finishedAt: string };
  providerStdinJournal?: ReturnType<typeof readProviderStdinJournal>;
  eventEvidence?: Awaited<ReturnType<typeof readEventEvidence>>;
  toolCallbacksVerified?: Awaited<ReturnType<typeof verifyToolCallback>>[];
  sqliteLockErrorsEvidence?: {
    owner: string;
    scope: string;
    backendPid: number | undefined;
    baseline: number | null;
    completed: number | null;
  };
  sqliteLockErrorDelta?: number | null;
  queueDrain?: ReturnType<typeof classifyStageQueues> & {
    snapshot: Schema["QueueSizes"] | null | undefined;
    afterSettledWorkerTurns: number;
    durableDeliveredExchanges: number;
    retainedInputs?: ReturnType<typeof classifyRetainedInputs>;
  };
  timingEvidence?: ReturnType<typeof summarizeEventTiming> & {
    providerThreadReplyMs?: ReturnType<typeof summarizeProviderThreadReplies>;
  };
  apiLatencyMs?: {
    create: ReturnType<typeof summary>;
    message: ReturnType<typeof summary>;
  };
  completeTurnDeliveryLatencyMs?: ReturnType<typeof summary>;
  workerStopApiLatencyMs?: ReturnType<typeof summary> & {
    agents: { agentId: string; elapsedMs: number }[];
  };
};
type Journal = {
  version: number;
  runId: string;
  origin: string;
  project: string;
  leadProfile: ScaleProfile;
  workerProfile: ScaleProfile;
  workerWorkspace: string;
  stages: Stage[];
  ownedAgents: OwnedAgent[];
  http: HttpRecord[];
  eventStream: { state: string; reconnects: number };
  cleanup: Cleanup;
  metricsGaps: string[];
  runBudget: {
    stages: { workers: number; deadlineMs: number }[];
    maxStageWorkers: number | undefined;
    maxExchanges: number;
    requestDeadlineMs: number;
    setupAndFinalRequestCountMax: number;
    preflightCommandCountMax: number;
    cleanupDeadlineMs: number;
    maxWallMs: number;
    maxToolBarrierSeconds: number;
    apiRequestsMax: number;
    cleanupApiRequestReserve: number;
    hostResourceCeilings: string;
    worktreeDiskLimitBytes?: number;
    freeDiskFloorBytes?: number;
  };
  updatedAt?: string;
  backendIdentity?: BackendIdentity;
  diskPreflight?: Awaited<ReturnType<typeof verifyDiskPreflight>>;
  projectHead?: string;
  ownerBaseline?: {
    at: string;
    workspaceId: string;
    generation: number | null;
    checkpoint: Schema["SyncCheckpoint"] | null;
    documentsObserved: number;
    note: string;
  };
  foreignRetainedInputBaseline?: {
    at: string;
    statuses: readonly string[];
    identities: RetainedInput[];
  };
  catalog?: {
    checkedAt: string;
    lead: { accountKeys: string[]; models: [string, string][] };
    workers: { accountKeys: string[]; models: [string, string][] };
  };
  providerStdinDiagnostics?: {
    enabled: boolean;
    environmentOwner: string;
    source: string;
    journalPrefix: string;
    pid: number;
    bootId: string | undefined;
  };
  result?: { state: string; error?: string; reason?: string; at: string };
  diagnosticsAfterCleanup?: DiagnosticSnapshot;
  metrics?: ReturnType<typeof aggregateMetrics>;
};
type StreamEvent = {
  name: string;
  value?: StateFrame;
  receivedAt?: number;
  stateDocuments?: Schema["SyncDocument"][];
};
type TimingEvidenceInput = {
  events?: EvidenceEvent[];
  providerErrorEvents?: {
    id: string;
    agentId: string;
    exchange: number;
    error?: string | undefined;
  }[];
};
type EvidenceEvent = {
  id: string;
  agentId: string;
  kind?: string;
  status?: string;
  exchange: number;
  error?: string;
  missing?: boolean;
  timing?: { enqueuedAt?: unknown };
  attempts?: AttemptEvidence[];
};

// The lead and worker profiles and the stage list are fixed once from the CLI.
// The default staged run is a Claude Sonnet lead over Codex Luna workers.
const DEFAULT_LEAD_PROFILE = "claude-sonnet";
const DEFAULT_WORKER_PROFILE = "codex-luna";
let LEAD = scaleProfile(DEFAULT_LEAD_PROFILE);
let WORKER = scaleProfile(DEFAULT_WORKER_PROFILE);
const STDIN_TIMING_DIAGNOSTIC_ENV = "CODEX_RUNTIME_STDIN_TIMING_DIAGNOSTICS";
const DEFAULT_STAGES = [30, 128, 512];
let STAGES = DEFAULT_STAGES;
// Where workers run: `shared` (project checkout) or `worktree` (one per worker).
let WORKER_WORKSPACE: ReturnType<typeof parseWorkerWorkspace> =
  parseWorkerWorkspace();
const STAGE_DEADLINE_MS = 5 * 60 * 1000;
const REQUEST_DEADLINE_MS = 30_000;
const SETUP_AND_FINAL_REQUESTS_MAX = 8;
// GNU timeout, journalctl, git ls-tree, git rev-parse, and the retained-input baseline read.
const PREFLIGHT_COMMANDS_MAX = 5;
const CLEANUP_DEADLINE_MS = 5 * 60 * 1000;
const STREAM_RECONNECTS = 0;
const API_P95_LIMIT_MS = 1_000;
const API_REQUESTS_MAX = 12_000;
let CLEANUP_API_REQUEST_RESERVE = ownedAgentsCleanupReserve(STAGES);
function ownedAgentsCleanupReserve(stages: number[]) {
  return stages.reduce((sum, workers) => sum + workers + 1, 0) * 6 + 32;
}
// Selects the stages to run, for example `512` or `30,128`.
export function parseStages(value: unknown) {
  const stages = String(value)
    .split(",")
    .map((item) => Number(item));
  assert.ok(
    stages.length > 0 &&
      stages.every(
        (item) => Number.isSafeInteger(item) && item > 0 && item <= 512,
      ),
    "--stages must be a comma list of worker counts from 1 to 512.",
  );
  return stages;
}
const WORKTREE_DISK_BUDGET_BYTES = 128 * 1024 ** 3;
const FREE_DISK_FLOOR_BYTES = 256 * 1024 ** 3;
// The server owns the page size; this checkpoint does not need a full entity seed.
export const ENTITY_BASELINE_PULL_PATH =
  "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1";
const journalWriteTails = new WeakMap<Journal, Promise<void>>();
const REPO_ROOT = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../..",
);
const workloadAbort = new AbortController();
const barrierProcesses = new Set<ChildProcess>();
let interruptRequested = false;
// Owned release helpers are stopped at once on an interrupt or a failed stage.
function killBarrierProcesses() {
  for (const child of barrierProcesses) {
    child.kill("SIGTERM");
    const killTimer = setTimeout(() => child.kill("SIGKILL"), 1_000);
    killTimer.unref();
  }
}

function parseArgs(argv: string[]) {
  const result: Record<string, string | boolean | undefined> = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--resume") {
      result.resume = true;
      continue;
    }
    if (arg === "--help" || arg === "-h") {
      result.help = true;
      continue;
    }
    assert.ok(arg);
    if (!arg.startsWith("--")) throw new Error(`Unexpected argument: ${arg}`);
    const key = arg.slice(2);
    const value = argv[i + 1];
    if (!value || value.startsWith("--"))
      throw new Error(`Missing value for ${arg}`);
    result[key] = value;
    i += 1;
  }
  return result;
}

function usage() {
  return [
    "Usage:",
    "  node scripts/verify-rust-chat-scale.ts --url http://127.0.0.1:4620 --project /existing/project --evidence-dir /outside/checkout/evidence [--lead-profile claude-sonnet] [--worker-profile codex-luna|claude-haiku] [--stages 30,128,512] [--worker-workspace shared|worktree]",
    "  node scripts/verify-rust-chat-scale.ts --url http://127.0.0.1:4620 --evidence-dir /outside/checkout/evidence --resume",
    "",
    "This is a live, paid provider load run. Run only as an announced standalone LOAD-class operation.",
  ].join("\n");
}

function saveJournal(path: string, value: Journal) {
  value.updatedAt = new Date().toISOString();
  const snapshot = `${JSON.stringify(value, null, 2)}\n`;
  const previous = journalWriteTails.get(value) || Promise.resolve();
  const nextWrite = previous.then(async () => {
    const next = `${path}.next`;
    await writeFile(next, snapshot, { mode: 0o600 });
    await rename(next, path);
  });
  journalWriteTails.set(
    value,
    nextWrite.catch(() => {}),
  );
  return nextWrite;
}

async function readJournal(path: string): Promise<Journal | null> {
  try {
    // This verifier reads the versioned ownership journal it wrote itself.
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    if (errorCode(error) === "ENOENT") return null;
    throw error;
  }
}

function validateJournal(journal: Journal) {
  assert.equal(
    journal.version,
    1,
    "Unsupported scale verifier journal version.",
  );
  assert.ok(
    typeof journal.runId === "string" &&
      /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
        journal.runId,
      ),
    "Scale verifier journal has no valid run ID.",
  );
  assert.ok(
    Array.isArray(journal.ownedAgents),
    "Journal ownership list is missing.",
  );
}

function deadlineSignal(ms: number, parentSignal: AbortSignal | null = null) {
  const controller = new AbortController();
  const timer = setTimeout(
    () => controller.abort(new Error(`Request exceeded ${ms} ms.`)),
    ms,
  );
  timer.unref?.();
  const abortFromParent = () => controller.abort(parentSignal?.reason);
  if (parentSignal?.aborted) abortFromParent();
  else parentSignal?.addEventListener("abort", abortFromParent, { once: true });
  return {
    signal: controller.signal,
    disarm: () => clearTimeout(timer),
    close: () => {
      clearTimeout(timer);
      parentSignal?.removeEventListener("abort", abortFromParent);
    },
  };
}

function percentile(values: number[], p: number) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.ceil(p * sorted.length) - 1] ?? null;
}

function summary(values: number[]) {
  return {
    samples: values.length,
    p50Ms: percentile(values, 0.5),
    p95Ms: percentile(values, 0.95),
    p99Ms: percentile(values, 0.99),
    maxMs: values.length ? Math.max(...values) : null,
  };
}

function safeOrigin(value: string) {
  const url = new URL(value);
  assert.ok(
    url.protocol === "http:" || url.protocol === "https:",
    "Target must use HTTP(S).",
  );
  assert.ok(
    ["127.0.0.1", "localhost", "::1"].includes(url.hostname),
    "Live scale verifier only targets the local shared backend.",
  );
  assert.equal(url.username, "");
  assert.equal(url.password, "");
  return url.origin;
}

function shellQuote(value: unknown) {
  return `'${String(value).replaceAll("'", "'\\''")}'`;
}

async function readFilesystemSnapshot(path: string) {
  const stats = await statfs(path, { bigint: true });
  const availableBytes = stats.bavail * stats.bsize;
  const usedBytes = (stats.blocks - stats.bfree) * stats.bsize;
  const totalBytes = stats.blocks * stats.bsize;
  for (const [name, value] of Object.entries({
    availableBytes,
    usedBytes,
    totalBytes,
  })) {
    assert.ok(
      value >= 0n && value <= BigInt(Number.MAX_SAFE_INTEGER),
      `Filesystem ${name} is outside the safe integer range.`,
    );
  }
  return {
    at: new Date().toISOString(),
    availableBytes: Number(availableBytes),
    usedBytes: Number(usedBytes),
    totalBytes: Number(totalBytes),
  };
}

// Studio places worker worktrees at <root>/codex-agents/<id>, where <root> is
// <repository>/.worktrees or the configured worktree root
// (runtime-services worktree_root.rs). Git registration proves ownership.
function ownedWorktreeRoot(cwd: string, agentId: string) {
  let current = resolve(cwd);
  while (current !== dirname(current)) {
    if (
      current.split(sep).filter(Boolean).at(-1) === agentId &&
      dirname(current).split(sep).filter(Boolean).at(-1) === "codex-agents"
    )
      return current;
    current = dirname(current);
  }
  throw new Error(`Owned worktree path is outside its managed root: ${cwd}`);
}

async function verifyDiskPreflight(project: string) {
  const tree = spawnSync(
    "git",
    ["-C", project, "ls-tree", "--full-tree", "-r", "-l", "HEAD"],
    {
      encoding: "utf8",
      timeout: REQUEST_DEADLINE_MS,
      maxBuffer: 8 * 1024 * 1024,
    },
  );
  assert.ok(
    !tree.error && tree.status === 0,
    `Could not estimate tracked worktree size: ${String((tree.error instanceof Error ? tree.error.message : undefined) || tree.stderr || tree.status).slice(0, 300)}`,
  );
  const head = spawnSync("git", ["-C", project, "rev-parse", "HEAD"], {
    encoding: "utf8",
    timeout: REQUEST_DEADLINE_MS,
  });
  assert.ok(
    !head.error &&
      head.status === 0 &&
      /^[0-9a-f]{40,64}$/i.test(head.stdout.trim()),
    `Could not freeze selected project HEAD: ${String((head.error instanceof Error ? head.error.message : undefined) || head.stderr || head.status).slice(0, 300)}`,
  );
  const trackedBytes = tree.stdout
    .trim()
    .split("\n")
    .reduce((sum, line) => {
      const metadata = line.slice(0, line.indexOf("\t")).split(/\s+/);
      if (metadata[1] === "commit") return sum;
      const size = Number(metadata[3]);
      assert.ok(
        Number.isSafeInteger(size) && size >= 0,
        "Could not parse tracked worktree size.",
      );
      return sum + size;
    }, 0);
  assert.ok(
    trackedBytes > 0,
    "The selected project has no tracked HEAD content to estimate worktree storage.",
  );
  // Shared workers add no worktrees; only each stage's orchestrator can own one.
  const maxOwnedWorktrees =
    (WORKER_WORKSPACE === "shared"
      ? 0
      : STAGES.reduce((sum, workers) => sum + workers, 0)) + STAGES.length;
  const estimatedBytesWith25PercentMargin = Math.ceil(
    trackedBytes * maxOwnedWorktrees * 1.25,
  );
  assert.ok(
    estimatedBytesWith25PercentMargin <= WORKTREE_DISK_BUDGET_BYTES,
    `Estimated worktree data requires ${estimatedBytesWith25PercentMargin} bytes, above the ${WORKTREE_DISK_BUDGET_BYTES}-byte budget.`,
  );
  const snapshot = await readFilesystemSnapshot(project);
  const requiredBytes = WORKTREE_DISK_BUDGET_BYTES + FREE_DISK_FLOOR_BYTES;
  assert.ok(
    snapshot.availableBytes >= requiredBytes,
    `Live scale run requires ${requiredBytes} free bytes before setup; observed ${snapshot.availableBytes}.`,
  );
  return {
    snapshot,
    projectHead: head.stdout.trim(),
    trackedBytesPerWorktree: trackedBytes,
    maxOwnedWorktrees,
    estimatedBytesWith25PercentMargin,
    worktreeDiskLimitBytes: WORKTREE_DISK_BUDGET_BYTES,
    freeDiskFloorBytes: FREE_DISK_FLOOR_BYTES,
    state: "passed",
  };
}

async function verifyOwnedWorktreeDisk({
  project,
  stageIds,
  owners,
  previousSnapshot,
  trackedBytesPerWorktree,
}: {
  project: string;
  stageIds: string[];
  owners: OwnedAgent[];
  previousSnapshot: FilesystemSnapshot;
  trackedBytesPerWorktree: number;
}) {
  const stageOwnerIds = new Set(stageIds);
  const stageOwners = owners.filter((owner) => stageOwnerIds.has(owner.id));
  assert.equal(
    stageOwners.length,
    stageIds.length,
    "A stage worker is missing its durable journal owner.",
  );
  const worktreeOwners = owners.filter(
    (owner): owner is OwnedAgent & { worktree: Worktree } =>
      Boolean(owner.worktree?.cwd),
  );
  const paths = worktreeOwners.map((owner) => owner.worktree.cwd);
  assert.equal(
    new Set(paths).size,
    paths.length,
    "Journal contains duplicate owned worktree paths.",
  );
  const projectInfo = await lstat(project);
  const worktreeVolumes = new Set<string>();
  for (const owner of worktreeOwners) {
    const root = ownedWorktreeRoot(owner.worktree.cwd, owner.id);
    owner.worktree.registeredRoot = root;
    const rootInfo = await lstat(root);
    assert.ok(
      rootInfo.isDirectory(),
      `Owned worktree root is not a directory: ${root}`,
    );
    // A configured root may be on another volume; account for it separately.
    if (rootInfo.dev !== projectInfo.dev) worktreeVolumes.add(dirname(root));
  }
  const estimatedBytesWith25PercentMargin = Math.ceil(
    trackedBytesPerWorktree * worktreeOwners.length * 1.25,
  );
  assert.ok(
    estimatedBytesWith25PercentMargin <= WORKTREE_DISK_BUDGET_BYTES,
    `Tracked-size worktree estimate requires ${estimatedBytesWith25PercentMargin} bytes, above the ${WORKTREE_DISK_BUDGET_BYTES}-byte budget.`,
  );
  const snapshot = await readFilesystemSnapshot(project);
  assert.ok(
    snapshot.availableBytes >= FREE_DISK_FLOOR_BYTES,
    `Free disk space fell below the ${FREE_DISK_FLOOR_BYTES}-byte floor.`,
  );
  for (const volume of worktreeVolumes) {
    const destination = await readFilesystemSnapshot(volume);
    assert.ok(
      destination.availableBytes >= FREE_DISK_FLOOR_BYTES,
      `Free disk space on the worktree volume ${volume} fell below the ${FREE_DISK_FLOOR_BYTES}-byte floor.`,
    );
  }
  return {
    at: new Date().toISOString(),
    stageIds,
    ownedWorktreeCount: paths.length,
    trackedBytesPerWorktree,
    estimatedBytesWith25PercentMargin,
    limitBytes: WORKTREE_DISK_BUDGET_BYTES,
    filesystemAvailableBytesBefore: previousSnapshot.availableBytes,
    filesystemAvailableBytesAfter: snapshot.availableBytes,
    filesystemUsedDeltaBytes: snapshot.usedBytes - previousSnapshot.usedBytes,
    filesystemDeltaAttribution:
      "shared-filesystem delta; not attributed solely to owned worktrees",
    snapshot,
    freeFloorBytes: FREE_DISK_FLOOR_BYTES,
    state: "passed",
  };
}

function parseStateEntity(
  document: Schema["SyncPullResponse"]["documents"][number] | undefined,
) {
  if (
    !document ||
    !("payload" in document) ||
    document._deleted ||
    typeof document.payload !== "string"
  )
    return null;
  try {
    const entity: Schema["SyncEntityPayload"] = JSON.parse(document.payload);
    return entity.collection === "agent" ? entity.value : null;
  } catch {
    return null;
  }
}

async function waitForCallbackReadiness(
  markers: ReadinessWatch,
  client: Client,
  ids: string[],
  runId: string,
  workers: number,
  tokens: string[],
  deadlineAt: number,
  project: string,
  journal: Journal,
  journalPath: string,
) {
  const ready = new Map<
    string,
    Barrier & { agentId: string; index: number; cwd: string; directory: string }
  >();
  const currentCwd = async (agentId: string) => {
    const entity = await client.agentSnapshot(agentId);
    return callbackWorktreeCwd(entity, agentId, project);
  };
  // A verified marker proves the FIFO shell opened its release FIFO, so the
  // barrier path is checked before the worker is accepted for release.
  while (ready.size < ids.length) {
    const index = await markers.next(deadlineAt);
    assert.ok(index !== undefined);
    const agentId = ids[index];
    const token = tokens[index];
    assert.ok(agentId && token);
    const owner = journal.ownedAgents.find((item) => item.id === agentId);
    assert.ok(owner, `Worker ${agentId} is not journal-owned.`);
    assert.ok(
      !ready.has(agentId),
      `Worker ${agentId} published its readiness marker twice.`,
    );
    // Persist the verified lifetime bound before a failed read or process exit.
    owner.callbackReadinessObservedAt = new Date().toISOString();
    await saveJournal(journalPath, journal);
    const shared = WORKER_WORKSPACE === "shared";
    const cwd = shared ? project : await currentCwd(agentId);
    assert.ok(
      shared || [...ready.values()].every((item) => resolve(item.cwd) !== cwd),
      `Worker ${agentId} shares a workspace path with another callback owner.`,
    );
    const { directory, path } = shared
      ? sharedBarrierPaths(await realpath(dirname(journalPath)), workers, index)
      : {
          directory: join(cwd, `.codex-scale-${runId}-${index}`),
          path: join(cwd, `.codex-scale-${runId}-${index}`, "release.fifo"),
        };
    assertOwnedEntry(
      await lstat(directory),
      directory,
      (info) => info.isDirectory(),
      0o700,
    );
    assertOwnedEntry(await lstat(path), path, (info) => info.isFIFO(), 0o600);
    ready.set(agentId, {
      agentId,
      index,
      cwd,
      directory,
      path,
      token,
    });
    const barrier = {
      barrierDirectory: directory,
      barrierPath: path,
      readinessMarkerPath: markers.path(index),
      completionTag: `STUDIO_SCALE_${runId.slice(0, 8)}_${workers}_${index}_TOOL_OK`,
      readinessObservedAt: owner.callbackReadinessObservedAt,
    };
    if (shared) owner.sharedBarrier = barrier;
    else owner.worktree = { cwd, cleanup: "pending", ...barrier };
  }
  assert.ok(
    WORKER_WORKSPACE === "shared" ||
      new Set([...ready.values()].map((item) => item.cwd)).size === ids.length,
    "Managed workers do not have unique worktree paths.",
  );
  return ids.map((id) => {
    const item = ready.get(id);
    assert.ok(item);
    return item;
  });
}

async function main({
  setRemaining,
}: Pick<ScaleVerifierLifecycle, "setRemaining">) {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(`${usage()}\n`);
    return;
  }
  assert.ok(options.url, "--url is required.");
  LEAD = scaleProfile(
    typeof options["lead-profile"] === "string"
      ? options["lead-profile"]
      : DEFAULT_LEAD_PROFILE,
  );
  WORKER = scaleProfile(
    typeof options["worker-profile"] === "string"
      ? options["worker-profile"]
      : DEFAULT_WORKER_PROFILE,
  );
  WORKER_WORKSPACE = parseWorkerWorkspace(
    typeof options["worker-workspace"] === "string"
      ? options["worker-workspace"]
      : undefined,
  );
  if (options.stages) {
    STAGES = parseStages(options.stages);
    CLEANUP_API_REQUEST_RESERVE = ownedAgentsCleanupReserve(STAGES);
  }
  assert.ok(options["evidence-dir"], "--evidence-dir is required.");
  if (!options.resume)
    assert.ok(options.project, "--project is required for a new live run.");
  assert.ok(typeof options.url === "string");
  assert.ok(typeof options["evidence-dir"] === "string");
  const origin = safeOrigin(options.url);
  const evidenceDir = resolve(options["evidence-dir"]);
  const journalPath = join(evidenceDir, "rust-chat-scale.json");
  if (!options.resume) {
    await mkdir(evidenceDir, { recursive: true, mode: 0o700 });
    await assertEvidenceOutsideCheckout(evidenceDir);
  }
  let prior = await readJournal(journalPath);
  if (options.resume) {
    await assertEvidenceOutsideCheckout(evidenceDir);
    assert.ok(
      prior,
      "--resume requires the existing durable ownership journal.",
    );
    validateJournal(prior);
    assert.equal(prior.origin, origin, "Evidence belongs to another backend.");
    const releaseLock = await acquireRunLock(
      `${journalPath}.lock`,
      prior.runId,
    );
    try {
      const lockedJournal = await readJournal(journalPath);
      assert.ok(
        lockedJournal,
        "Ownership journal disappeared after lock acquisition.",
      );
      validateJournal(lockedJournal);
      assert.equal(
        lockedJournal.runId,
        prior.runId,
        "Ownership journal changed to another run after lock acquisition.",
      );
      assert.equal(
        lockedJournal.origin,
        origin,
        "Evidence belongs to another backend.",
      );
      prior = lockedJournal;
      const resumedJournal = prior;
      setRemaining(() => remainingOwnership(journalPath, resumedJournal));
      await resumeCleanup(origin, journalPath, prior);
    } finally {
      await releaseLock();
    }
    return;
  }
  assert.equal(
    prior,
    null,
    "Evidence directory already has an attempt; inspect it or use --resume.",
  );
  assert.ok(typeof options.project === "string");
  const project = await realpath(resolve(options.project));
  const projectInfo = await lstat(project);
  assert.ok(
    projectInfo.isDirectory(),
    "--project must name an existing directory.",
  );
  const runId = randomUUID();
  const releaseLock = await acquireRunLock(`${journalPath}.lock`, runId);
  try {
    prior = await readJournal(journalPath);
    assert.equal(
      prior,
      null,
      "Evidence journal appeared while acquiring the exclusive run lock; inspect it or use --resume.",
    );
  } catch (error) {
    await releaseLock();
    throw error;
  }
  const journal: Journal = {
    version: 1,
    runId,
    origin,
    project,
    leadProfile: LEAD,
    workerProfile: WORKER,
    workerWorkspace: WORKER_WORKSPACE,
    stages: [],
    ownedAgents: [],
    http: [],
    eventStream: { state: "starting", reconnects: STREAM_RECONNECTS },
    cleanup: { state: "pending", agents: [] },
    metricsGaps: [
      "Provider generation time has no provider-owned start/end timing in the current diagnostics contract; observed whole-turn delivery is reported separately.",
      "Upstream limits can be identified from explicit per-turn errors, but the current diagnostics contract has no aggregate provider-specific rate-limit counter.",
      "Per-worker stop API latency measures post-workload cleanup only; it does not measure active interrupt latency.",
    ],
    runBudget: {
      stages: STAGES.map((workers) => ({
        workers,
        deadlineMs: STAGE_DEADLINE_MS,
      })),
      maxStageWorkers: STAGES.at(-1),
      maxExchanges: STAGES.reduce((sum, workers) => sum + workers * 2, 0),
      requestDeadlineMs: REQUEST_DEADLINE_MS,
      setupAndFinalRequestCountMax: SETUP_AND_FINAL_REQUESTS_MAX,
      preflightCommandCountMax: PREFLIGHT_COMMANDS_MAX,
      cleanupDeadlineMs: CLEANUP_DEADLINE_MS,
      maxWallMs:
        STAGES.length * STAGE_DEADLINE_MS +
        CLEANUP_DEADLINE_MS +
        (SETUP_AND_FINAL_REQUESTS_MAX + PREFLIGHT_COMMANDS_MAX) *
          REQUEST_DEADLINE_MS,
      maxToolBarrierSeconds:
        TOOL_BARRIER_MAX_SECONDS + TOOL_BARRIER_KILL_GRACE_SECONDS,
      apiRequestsMax: API_REQUESTS_MAX,
      cleanupApiRequestReserve: CLEANUP_API_REQUEST_RESERVE,
      hostResourceCeilings:
        "backend/provider process RSS is host-dependent and not hard-capped by this verifier",
    },
  };
  setRemaining(() => remainingOwnership(journalPath, journal));
  await saveJournal(journalPath, journal).catch(async (error) => {
    await releaseLock();
    throw error;
  });
  const client = await createClient(origin, journal, journalPath).catch(
    async (error) => {
      await releaseLock();
      throw error;
    },
  );
  let stream: SyncStream | null | undefined;
  let readinessWatch: ReadinessWatch | null = null;
  let resultError: unknown;
  let previousDiskSnapshot: FilesystemSnapshot | undefined;
  try {
    const accounts = (
      await client.get<Schema["AccountsResponse"]>("/api/accounts")
    ).accounts;
    const resolveRole = async (profile: ScaleProfile) => {
      const accountKeys = scaleAccountKeys(accounts, profile);
      const modelByAccount = new Map<string, string>();
      for (const key of accountKeys) {
        const catalog = await client.get<Schema["ModelCatalogResponse"]>(
          `/api/models?account_key=${encodeURIComponent(key)}`,
        );
        const candidate = requireScaleModel(catalog, profile);
        // Pin paid requests to the catalog's resolved model identity.
        const model = candidate.resolvedModel ?? candidate.model;
        assert.ok(typeof model === "string");
        modelByAccount.set(key, model);
      }
      return { profile, accountKeys, modelByAccount };
    };
    const leadRole = await resolveRole(LEAD);
    const workerRole = await resolveRole(WORKER);
    const leadAccount = leadRole.accountKeys[0];
    const workerDefaultAccount = workerRole.accountKeys[0];
    assert.ok(leadAccount && workerDefaultAccount);
    const leadModel = leadRole.modelByAccount.get(leadAccount);
    const workerDefaultModel =
      workerRole.modelByAccount.get(workerDefaultAccount);
    assert.ok(leadModel && workerDefaultModel);
    assert.equal(
      process.platform,
      "linux",
      "This verifier requires Linux and GNU coreutils timeout.",
    );
    const timeoutVersion = spawnSync("timeout", ["--version"], {
      encoding: "utf8",
      timeout: REQUEST_DEADLINE_MS,
    });
    assert.ok(
      timeoutVersion.status === 0 &&
        /GNU coreutils/.test(timeoutVersion.stdout),
      "GNU coreutils timeout is required before any provider-backed agent is created.",
    );
    const journalVersion = spawnSync("journalctl", ["--version"], {
      encoding: "utf8",
      timeout: REQUEST_DEADLINE_MS,
    });
    assert.ok(
      journalVersion.status === 0 && /systemd/.test(journalVersion.stdout),
      "systemd journalctl is required for exact provider stdin timing evidence.",
    );
    journal.diskPreflight = await verifyDiskPreflight(project);
    previousDiskSnapshot = journal.diskPreflight.snapshot;
    journal.projectHead = journal.diskPreflight.projectHead;
    journal.runBudget.worktreeDiskLimitBytes = WORKTREE_DISK_BUDGET_BYTES;
    journal.runBudget.freeDiskFloorBytes = FREE_DISK_FLOOR_BYTES;
    journal.catalog = {
      checkedAt: new Date().toISOString(),
      lead: {
        accountKeys: leadRole.accountKeys,
        models: [...leadRole.modelByAccount],
      },
      workers: {
        accountKeys: workerRole.accountKeys,
        models: [...workerRole.modelByAccount],
      },
    };
    process.stdout.write(
      `${JSON.stringify({ loadClass: true, startsAfterThisAnnouncement: true, workersByStage: STAGES, maxWallMs: journal.runBudget.maxWallMs, providerWorkerTurnsMax: journal.runBudget.maxExchanges, providerLeadTurnsMax: STAGES.length, providerTurnsMax: journal.runBudget.maxExchanges + STAGES.length, apiRequestsMax: journal.runBudget.apiRequestsMax, cleanupApiRequestReserve: CLEANUP_API_REQUEST_RESERVE, maxConcurrentToolCallbacks: STAGES.at(-1), toolChildrenWaitOnKernelFifo: true, toolCallbackReadinessOutput: true, providerStdinTimingDiagnostic: STDIN_TIMING_DIAGNOSTIC_ENV, journalEvidenceScope: "backend PID + kernel boot ID + stage wall-clock", worktreeDiskLimitBytes: WORKTREE_DISK_BUDGET_BYTES, trackedSizeEstimateBytes: journal.diskPreflight.estimatedBytesWith25PercentMargin, freeDiskFloorBytes: FREE_DISK_FLOOR_BYTES, initialFreeDiskBytes: journal.diskPreflight.snapshot.availableBytes, diskAccounting: "tracked-size estimate, causal statfs deltas, and owned worktree root metadata; no recursive walk", syntheticCpuMemoryDiskNetworkLoad: false, hostResourceCeilings: journal.runBudget.hostResourceCeilings, leadProfile: LEAD, workerProfile: WORKER, workerWorkspace: WORKER_WORKSPACE, workerAccounts: workerRole.accountKeys })}\n`,
    );
    const desktop = await client.get<Schema["DesktopResponse"]>("/api/desktop");
    assert.equal(
      "backendKind" in desktop ? desktop.backendKind : undefined,
      "rust",
      "The target is not the shared Rust backend required by this verifier.",
    );
    assert.ok(
      typeof desktop.stateDir === "string" && desktop.stateDir.length > 0,
      "The local Rust backend did not identify its state directory.",
    );
    assert.ok(
      Number.isSafeInteger(desktop.pid) && desktop.pid > 0,
      "The local Rust backend did not identify its process.",
    );
    journal.backendIdentity = {
      pid: desktop.pid,
      stateDir: resolve(desktop.stateDir),
      backendKind:
        "backendKind" in desktop && typeof desktop.backendKind === "string"
          ? desktop.backendKind
          : null,
      backendBuild: desktop.backendBuild || null,
    };
    journal.backendIdentity.bootId = canonicalJournalBootId(
      (await readFile("/proc/sys/kernel/random/boot_id", "utf8")).trim(),
    );
    const backendEnvironment = await readFile(`/proc/${desktop.pid}/environ`);
    const timingDiagnosticsEnabled = backendEnvironment
      .toString("utf8")
      .split("\0")
      .includes(`${STDIN_TIMING_DIAGNOSTIC_ENV}=1`);
    assert.ok(
      timingDiagnosticsEnabled,
      `The Rust backend must start with ${STDIN_TIMING_DIAGNOSTIC_ENV}=1 before paid provider writes.`,
    );
    journal.providerStdinDiagnostics = {
      enabled: true,
      environmentOwner: STDIN_TIMING_DIAGNOSTIC_ENV,
      source:
        "backend /proc environment checked without recording other variables",
      journalPrefix: "studio_provider_stdin_write ",
      pid: desktop.pid,
      bootId: journal.backendIdentity.bootId,
    };
    const baseline = await client.get<Schema["SyncPullResponse"]>(
      ENTITY_BASELINE_PULL_PATH,
    );
    assert.ok(
      typeof baseline.workspaceId === "string" && baseline.workspaceId,
      "Initial sync baseline did not include a workspace identity.",
    );
    journal.ownerBaseline = {
      at: new Date().toISOString(),
      workspaceId: baseline.workspaceId,
      generation: baseline.generation || null,
      checkpoint: baseline.checkpoint || null,
      documentsObserved: (baseline.documents || []).length,
      note: "Initial authoritative entity-sync checkpoint saved before any verifier writes; every UUID is also checked individually before create.",
    };
    const syncIdentity =
      await client.get<Schema["SyncIdentityResponse"]>("/api/sync/identity");
    assert.equal(
      syncIdentity.workspaceId,
      journal.ownerBaseline.workspaceId,
      "Sync identity changed while establishing the ownership baseline.",
    );
    // Foreign retained inputs are fixed before any verifier write. Identities,
    // agents, kinds, and statuses only; message text is never read into evidence.
    journal.foreignRetainedInputBaseline = {
      at: new Date().toISOString(),
      statuses: RETAINED_INPUT_STATUSES,
      identities: readRetainedInputRows(
        journal.backendIdentity.stateDir,
        journal.ownerBaseline.workspaceId,
        performance.now() + REQUEST_DEADLINE_MS,
      ),
    };
    await saveJournal(journalPath, journal);
    stream = await startSyncResourceStream(client, journal, journalPath);
    journal.eventStream.state = "connected";
    await saveJournal(journalPath, journal);
    for (const workers of STAGES) {
      const stage = makeStage(workers);
      journal.stages.push(stage);
      await saveJournal(journalPath, journal);
      const stageStarted = performance.now();
      const stageDeadline = stageStarted + STAGE_DEADLINE_MS;
      client.setDeadline(stageDeadline);
      stage.state = "creating";
      stage.startedAt = new Date().toISOString();
      const barrierTokens = Array.from({ length: workers }, () => randomUUID());
      const readinessTokens = Array.from({ length: workers }, () =>
        randomUUID(),
      );
      stage.toolBarrierTokens = barrierTokens;
      const stageBackend =
        await client.get<Schema["DesktopResponse"]>("/api/desktop");
      assert.equal(
        "backendKind" in stageBackend ? stageBackend.backendKind : undefined,
        "rust",
      );
      assert.equal(
        resolve(stageBackend.stateDir),
        journal.backendIdentity.stateDir,
        "The backend state directory changed during the staircase.",
      );
      const stageIdentity =
        await client.get<Schema["SyncIdentityResponse"]>("/api/sync/identity");
      assert.equal(
        stageIdentity.workspaceId,
        journal.ownerBaseline.workspaceId,
        "The backend workspace identity changed during the staircase.",
      );
      stage.backendPid = stageBackend.pid;
      stage.diagnosticsBefore = await diagnostics(client);
      if (
        stage.diagnosticsBefore.processTree &&
        Number.isSafeInteger(stage.diagnosticsBefore.processTree.rootPid)
      )
        assert.equal(
          stage.diagnosticsBefore.processTree.rootPid,
          stage.backendPid,
          "Diagnostics and /api/desktop identified different backend processes.",
        );
      await saveJournal(journalPath, journal);

      stage.toolBarrierRelease = { state: "waiting-for-overlap" };
      await saveJournal(journalPath, journal);

      // One orchestrator owns the managed child workers. Every shell callback
      // publishes its private readiness marker then blocks on a FIFO inside
      // its own worktree; release waits for full native-turn overlap.
      const leadId = randomUUID();
      const ids = Array.from({ length: workers }, () => randomUUID());
      client.observedAgentIds.add(leadId);
      for (const id of ids) client.observedAgentIds.add(id);
      stage.leadId = leadId;
      stage.agentIds = ids;
      // The marker watch is armed before any model-backed create request.
      stage.readinessMarkerDirectory = await createReadinessMarkerDirectory(
        join(evidenceDir, `readiness-markers-${workers}`),
      );
      readinessWatch = await watchReadinessMarkers(
        stage.readinessMarkerDirectory,
        readinessTokens,
        workloadAbort.signal,
      );
      // Shared workers keep their FIFOs and command scripts under the owned
      // evidence directory, one private directory per stage.
      const barrierRoot = await realpath(evidenceDir);
      stage.workerScriptDirectory = await createReadinessMarkerDirectory(
        join(evidenceDir, `worker-scripts-${workers}`),
      );
      if (WORKER_WORKSPACE === "shared")
        stage.sharedBarrierDirectory = await createReadinessMarkerDirectory(
          join(evidenceDir, `fifos-${workers}`),
        );
      stage.createLatenciesMs = [];
      stage.createStartedAt = new Date().toISOString();
      const leadAbsent = await client.agentSnapshot(leadId);
      assert.equal(
        leadAbsent,
        null,
        `Generated lead identity already exists: ${leadId}`,
      );
      const createResults = [];
      let leadCreated = false;
      try {
        const lead = await client.write(
          "POST",
          "/api/agents",
          {
            id: leadId,
            cwd: project,
            prompt: `This is the scale test orchestrator for ${workers} managed workers. Reply with only STUDIO_SCALE_LEAD_${runId.slice(0, 8)}_${workers}.`,
            name: `scale-lead-${workers}`,
            role: "orchestrator",
            model: leadModel,
            effort: LEAD.effort,
            fast_mode: LEAD.fastMode,
            yolo_mode: true,
            account_key: leadAccount,
            concurrency: workers,
            maxAgents: workers + 1,
            worker_defaults: scaleWorkerDefaults(
              workerDefaultModel,
              WORKER,
              workerRole.accountKeys,
            ),
          },
          { type: "create", id: leadId, stageWorkers: workers, isLead: true },
        );
        stage.createLatenciesMs.push(lead.elapsedMs);
        leadCreated = true;
        createResults.push({ status: "fulfilled", value: lead });
        stage.leadExchangeCompleted = false;
        await waitForStageSettled(
          stream,
          [leadId],
          stageDeadline,
          stage,
          journal,
          journalPath,
        );
        const leadState = await client.agentSnapshot(leadId);
        assertStoredScaleProfile(leadState, {
          id: leadId,
          model: leadModel,
          effort: LEAD.effort,
          fastMode: LEAD.fastMode,
        });
        stage.leadServedModel = await verifyServedModel(
          stream,
          journal,
          LEAD,
          [leadId],
          1,
          stageDeadline,
        );
        if (
          leadState &&
          "worktreeReady" in leadState &&
          leadState.worktreeReady === true
        ) {
          assert.equal(leadState.workspaceMode, "worktree");
          assert.ok(
            typeof leadState.cwd === "string" &&
              isAbsolute(leadState.cwd) &&
              resolve(leadState.cwd) !== project,
            "Orchestrator worktree state has no isolated absolute cwd.",
          );
          const leadOwner = journal.ownedAgents.find(
            (owner) => owner.id === leadId,
          );
          assert.ok(leadOwner, "Orchestrator is not journal-owned.");
          leadOwner.worktree = {
            cwd: resolve(leadState.cwd),
            branch:
              "branch" in leadState && typeof leadState.branch === "string"
                ? leadState.branch
                : undefined,
            cleanup: "pending",
          };
          await saveJournal(journalPath, journal);
        }
        stage.leadExchangeCompleted = true;
      } catch (error) {
        if (leadCreated)
          stage.leadExchangeError = String(
            (error instanceof Error ? error.message : undefined) || error,
          );
        else createResults.push({ status: "rejected", reason: error });
      }
      if (
        createResults[0]?.status === "fulfilled" &&
        stage.leadExchangeCompleted
      ) {
        createResults.push(
          ...(await Promise.allSettled(
            ids.map(async (id, index) => {
              // Workers are spread round-robin over the profile's ready accounts.
              const workerAccount =
                workerRole.accountKeys[index % workerRole.accountKeys.length];
              assert.ok(workerAccount);
              const workerModel = workerRole.modelByAccount.get(workerAccount);
              assert.ok(workerModel);
              const tag = `STUDIO_SCALE_${runId.slice(0, 8)}_${workers}_${index}`;
              const shared = WORKER_WORKSPACE === "shared";
              const barrier = shared
                ? sharedBarrierPaths(barrierRoot, workers, index)
                : {
                    directory: `.codex-scale-${runId}-${index}`,
                    path: `.codex-scale-${runId}-${index}/release.fifo`,
                  };
              const token = barrierTokens[index];
              assert.ok(readinessWatch && stage.workerScriptDirectory);
              const markerPath = readinessWatch.path(index);
              const markerTemp = `${markerPath}.tmp`;
              // The marker is renamed into place only after the FIFO is open.
              const inner = `set -eu; (umask 077; mkdir ${shellQuote(barrier.directory)}); mkfifo -m 600 ${shellQuote(barrier.path)}; exec 3<>${shellQuote(barrier.path)}; (umask 077; printf '%s\\n' ${shellQuote(readinessTokens[index])} > ${shellQuote(markerTemp)}; mv -f ${shellQuote(markerTemp)} ${shellQuote(markerPath)}); IFS= read -r release <&3; [ "$release" = ${shellQuote(token)} ]; exec 3>&-; rm ${shellQuote(barrier.path)}; rmdir ${shellQuote(barrier.directory)}; printf '%s\\n' ${shellQuote(`${tag}_TOOL_OK`)}`;
              // The model gets one short command to run unchanged: the
              // script lives in an owned file, so there is little to rewrite.
              const scriptPath = join(
                stage.workerScriptDirectory,
                `worker-${index}.sh`,
              );
              await writeFile(scriptPath, `${inner}\n`, { mode: 0o600 });
              const command = `timeout -k ${TOOL_BARRIER_KILL_GRACE_SECONDS}s ${TOOL_BARRIER_MAX_SECONDS}s sh ${shellQuote(scriptPath)}`;
              const prompt = `Run this one shell command exactly as written, without changing, wrapping, or inspecting it. It blocks until it is released and then prints ${tag}_TOOL_OK. When it finishes, reply with only ${tag}. Command: ${command}`;
              const absent = await client.agentSnapshot(id);
              assert.equal(
                absent,
                null,
                `Generated identity already exists: ${id}`,
              );
              const write = await client.write(
                "POST",
                "/api/agents",
                {
                  id,
                  parent: leadId,
                  cwd: project,
                  workspaceMode: WORKER_WORKSPACE,
                  prompt,
                  name: `scale-${workers}-${index}`,
                  role: "implementer",
                  model: workerModel,
                  effort: WORKER.effort,
                  fast_mode: WORKER.fastMode,
                  account_key: workerAccount,
                },
                {
                  type: "create",
                  id,
                  parentId: leadId,
                  stageWorkers: workers,
                  stageIndex: index,
                },
              );
              stage.createLatenciesMs.push(write.elapsedMs);
              assertStoredScaleProfile(await client.agentSnapshot(id), {
                id,
                model: workerModel,
                effort: WORKER.effort,
                fastMode: WORKER.fastMode,
              });
              stage.createsConfirmed = (stage.createsConfirmed || 0) + 1;
            }),
          )),
        );
      }
      stage.createResults = createResults.map((item, index) => ({
        id: index === 0 ? leadId : ids[index - 1],
        state: item.status,
        error:
          item.status === "rejected"
            ? String(
                (item.reason instanceof Error
                  ? item.reason.message
                  : undefined) || item.reason,
              ).slice(0, 500)
            : null,
      }));
      await saveJournal(journalPath, journal);
      if (stage.leadExchangeError)
        throw new Error(
          `Stage ${workers} orchestrator exchange failed before child creation: ${stage.leadExchangeError}`,
        );
      if (createResults.some((item) => item.status === "rejected"))
        throw new Error(
          `Stage ${workers} agent creation had an unknown or failed response; no create write is retried.`,
        );

      // Child completions can wake their lead. Stop the now-idle lead after
      // all children exist, while leaving descendants running independently.
      stage.leadWakeSuppression = {
        state: "prepared",
        intentAt: new Date().toISOString(),
      };
      await saveJournal(journalPath, journal);
      const leadStop = await client.request("POST", "/api/stop", {
        id: leadId,
        descendants: false,
      });
      const stoppedLead = await client.agentSnapshot(leadId);
      assert.ok(
        stoppedLead && !stoppedLead.inFlight && stoppedLead.autoWake === false,
        "Lead auto-wake suppression was not confirmed before worker turns.",
      );
      stage.leadWakeSuppression = {
        state: "confirmed",
        elapsedMs: leadStop.elapsedMs,
        confirmedAt: new Date().toISOString(),
        leadStatus: stoppedLead.status,
        autoWake: stoppedLead.autoWake,
      };
      await saveJournal(journalPath, journal);

      stage.state = "waiting-for-concurrent-provider-turns";
      // The observer keeps draining state events (turn intervals are evidence
      // only) until every worker has published its readiness marker.
      const observer = observeWorkerTurns(
        stream,
        ids,
        stageDeadline,
        stage,
        journal,
        journalPath,
      );
      observer.catch(() => {});
      let readiness;
      try {
        readiness = await waitForCallbackReadiness(
          readinessWatch,
          client,
          ids,
          runId,
          workers,
          barrierTokens,
          stageDeadline,
          project,
          journal,
          journalPath,
        );
      } finally {
        stream.interrupt();
      }
      await observer;
      stage.toolCallbacksReadyCount = readiness.length;
      assert.equal(
        stage.toolCallbacksReadyCount,
        workers,
        "Not every worker reached its blocked tool callback before release.",
      );
      // Concurrency criterion: every worker turn reached the model and every
      // tool process is alive at once, still holding its release FIFO.
      stage.concurrentWorkers = assertConcurrentWorkers({
        workers,
        journalEntries: readProviderStdinJournal(
          journal.backendIdentity,
          {
            providerStdinJournalWindow: {
              startedAt: stage.startedAt,
              finishedAt: new Date().toISOString(),
            },
          },
          stageDeadline,
        ).entries,
        fifoPaths: readiness.map((item) => item.path),
        holders: findFifoHolders(readiness.map((item) => item.path)),
      });
      // Record each provider CLI process's memory and CPU so the run shows
      // whether the stage fits the host.
      stage.providerProcessesAtPeak = sampleProviderProcesses(
        readProcessTable(),
        stage.backendPid,
        WORKER.processName,
      );
      stage.worktreeDisk = await verifyOwnedWorktreeDisk({
        project,
        stageIds: stage.agentIds,
        owners: journal.ownedAgents,
        previousSnapshot: previousDiskSnapshot,
        trackedBytesPerWorktree: journal.diskPreflight.trackedBytesPerWorktree,
      });
      previousDiskSnapshot = stage.worktreeDisk.snapshot;
      readinessWatch.close();
      readinessWatch = null;
      stage.toolBarrierRelease = {
        state: "prepared",
        intentAt: new Date().toISOString(),
      };
      await saveJournal(journalPath, journal);
      stage.toolBarrierRelease.state = "releasing";
      await saveJournal(journalPath, journal);
      stage.toolBarrierReleasePaths = readiness.map((item) => item.path);
      stage.toolBarrierWorkerDirectories = readiness.map(
        (item) => item.directory,
      );
      stage.toolBarrierState = "ready-in-worker-worktrees";
      await releaseFifoBarriers(readiness, stageDeadline, barrierProcesses);
      stage.toolBarrierRelease.state = "confirmed";
      stage.toolBarrierRelease.confirmedAt = new Date().toISOString();
      stage.barrierReleasedAt = new Date().toISOString();
      await saveJournal(journalPath, journal);
      stage.state = "waiting-first-exchanges";
      await waitForStageSettled(
        stream,
        ids,
        stageDeadline,
        stage,
        journal,
        journalPath,
      );
      // Served-model proof gates the paid second exchanges: no further turn
      // is sent unless the first ones were served by Haiku.
      stage.firstExchangeServedModel = await verifyServedModel(
        stream,
        journal,
        WORKER,
        ids,
        1,
        stageDeadline,
      );
      for (const item of readiness) {
        for (const path of [item.path, item.directory]) {
          try {
            await lstat(path);
            throw new Error(
              `Worker callback left a run-owned barrier path behind: ${path}`,
            );
          } catch (error) {
            if (errorCode(error) !== "ENOENT") throw error;
          }
        }
      }
      stage.toolBarrierState = "worker-barriers-removed";
      await saveJournal(journalPath, journal);
      stage.firstExchangeCount = workers;
      stage.secondExchangeLatenciesMs = [];
      const secondExchangeIds = ids.map(() => randomUUID());
      stage.secondExchangeIds = secondExchangeIds;
      stage.state = "sending-second-exchanges";
      const secondResults = await Promise.allSettled(
        ids.map(async (agentId, index) => {
          const messageId = secondExchangeIds[index];
          assert.ok(messageId);
          const before = await client.receipt(agentId, messageId);
          assert.equal(
            before,
            null,
            `Second exchange identity already has a receipt: ${messageId}`,
          );
          const sent = await client.write(
            "POST",
            "/api/messages",
            {
              id: messageId,
              room: agentId,
              text: `Reply with only STUDIO_SCALE_SECOND_${runId.slice(0, 8)}_${workers}_${index}.`,
              delivery: "queue",
            },
            { type: "message", id: messageId, agentId, stageWorkers: workers },
          );
          stage.secondExchangeLatenciesMs.push(sent.elapsedMs);
        }),
      );
      stage.secondExchangeResults = secondResults.map((item, index) => ({
        id: secondExchangeIds[index],
        state: item.status,
        error:
          item.status === "rejected"
            ? String(
                (item.reason instanceof Error
                  ? item.reason.message
                  : undefined) || item.reason,
              ).slice(0, 500)
            : null,
      }));
      await saveJournal(journalPath, journal);
      if (secondResults.some((item) => item.status === "rejected"))
        throw new Error(
          `Stage ${workers} second exchange had an unknown or failed response; no message write is retried.`,
        );
      stage.state = "waiting-second-exchanges";
      await waitForStageSettled(
        stream,
        ids,
        stageDeadline,
        stage,
        journal,
        journalPath,
      );
      stage.providerStdinJournalWindow = {
        startedAt: stage.startedAt,
        finishedAt: new Date().toISOString(),
      };
      stage.secondExchangeServedModel = await verifyServedModel(
        stream,
        journal,
        WORKER,
        ids,
        2,
        stageDeadline,
      );
      stage.secondExchangeCount = workers;
      stage.eventEvidence = await readEventEvidence(
        journal.backendIdentity.stateDir,
        journal.ownerBaseline.workspaceId,
        stage,
        stageDeadline,
      );
      const journalBootId = canonicalJournalBootId(
        (await readFile("/proc/sys/kernel/random/boot_id", "utf8")).trim(),
      );
      assert.equal(
        journalBootId,
        canonicalJournalBootId(journal.backendIdentity.bootId),
        "Kernel boot identity changed while collecting provider stdin timing.",
      );
      stage.providerStdinJournal = readProviderStdinJournal(
        journal.backendIdentity,
        stage,
        stageDeadline,
      );
      const nativeTurnCounts = stage.eventEvidence.nativeTurnCounts;
      assert.deepEqual(
        Object.keys(nativeTurnCounts).sort(),
        [leadId, ...ids].sort(),
        "Read-only SQLite snapshot omitted an exact stage agent identity.",
      );
      stage.nativeTurnCounts = nativeTurnCounts;
      assert.ok(
        ids.every((id) => nativeTurnCounts[id] === 2),
        `Stage ${workers} did not durably complete exactly two native turns per worker.`,
      );
      stage.leadNativeTurnCount = nativeTurnCounts[stage.leadId] || 0;
      assert.equal(
        stage.leadNativeTurnCount,
        1,
        `Stage ${workers} lead durably completed an unexpected number of native turns.`,
      );
      const toolCallbacksVerified: Awaited<
        ReturnType<typeof verifyToolCallback>
      >[] = Array(ids.length);
      stage.toolCallbacksVerified = toolCallbacksVerified;
      await forEachBounded(ids, 32, async (agentId, index) => {
        toolCallbacksVerified[index] = await verifyToolCallback(
          client,
          agentId,
          runId,
          workers,
          index,
        );
      });
      assert.equal(
        stage.eventEvidence.missingCount,
        0,
        `Stage ${workers} is missing durable rows for exact worker exchange identities.`,
      );
      assert.equal(
        stage.eventEvidence.events.length,
        workers * 2,
        `Stage ${workers} does not have exactly two durable exchange rows per worker.`,
      );
      assert.ok(
        stage.eventEvidence.events.every(
          (event) => event.status === "delivered",
        ),
        `Stage ${workers} has an exchange that was not durably delivered.`,
      );
      stage.diagnosticsAfter = await diagnostics(client);
      const completedStageBackend =
        await client.get<Schema["DesktopResponse"]>("/api/desktop");
      assert.equal(
        completedStageBackend.pid,
        stage.backendPid,
        "Backend process changed before the stage diagnostics snapshot completed.",
      );
      const lockErrorsBefore = observedSqliteLockErrors(
        stage.diagnosticsBefore,
      );
      const lockErrorsAfter = observedSqliteLockErrors(stage.diagnosticsAfter);
      stage.sqliteLockErrorsEvidence = {
        owner: "Rust runtime-store typed StoreError boundary counter",
        scope:
          "Terminal SQLITE_BUSY/SQLITE_LOCKED errors converted through typed store boundaries; service-specific conversions and raw ReadConnection errors are excluded.",
        backendPid: stage.backendPid,
        baseline: lockErrorsBefore,
        completed: lockErrorsAfter,
      };
      stage.sqliteLockErrorDelta =
        lockErrorsBefore !== null &&
        lockErrorsAfter !== null &&
        lockErrorsAfter >= lockErrorsBefore
          ? lockErrorsAfter - lockErrorsBefore
          : null;
      if (stage.sqliteLockErrorDelta === null)
        journal.metricsGaps.push(
          `Stage ${workers}: the fresh-backend SQLite typed-boundary lock-error counter was missing or moved backwards.`,
        );
      else if (stage.sqliteLockErrorDelta > 0)
        throw new Error(
          `Stage ${workers} observed ${stage.sqliteLockErrorDelta} terminal SQLite busy/locked error(s) at the typed store boundary.`,
        );
      // Non-durable queues must drain to zero. The durable total spans every
      // agent, so retained inputs are classified by exact identity instead:
      // foreign inputs must equal the baseline, and owned inputs may remain
      // only as a paused parent's pending child-result notices.
      const queueSnapshot = stage.diagnosticsAfter.queues;
      const queueClass = classifyStageQueues(queueSnapshot);
      stage.queueDrain = {
        state: queueClass.state,
        snapshot: queueSnapshot,
        nonzero: queueClass.nonzero,
        unknown: queueClass.unknown,
        missing: queueClass.missing,
        durableInputPending: queueClass.durableInputPending,
        afterSettledWorkerTurns: workers * 2,
        durableDeliveredExchanges: stage.eventEvidence.events.length,
      };
      stage.queueDrain.retainedInputs = classifyRetainedInputs({
        rows: stage.eventEvidence.retainedInputs,
        baseline: journal.foreignRetainedInputBaseline.identities,
        ownedAgentIds: new Set(journal.ownedAgents.map((owner) => owner.id)),
        parentAgentIds: new Set(
          journal.stages.flatMap((item) => (item.leadId ? [item.leadId] : [])),
        ),
        phase: "beforeCleanup",
      });
      if (queueClass.state === "unavailable")
        journal.metricsGaps.push(
          `Stage ${workers}: diagnostic queue depths were missing, null, invalid, or unrecognized: ${JSON.stringify({ missing: queueClass.missing, unknown: queueClass.unknown })}.`,
        );
      assert.notEqual(
        queueClass.state,
        "unavailable",
        `Stage ${workers} diagnostic queue depths are unavailable.`,
      );
      assert.equal(
        queueClass.nonzero.length,
        0,
        `Stage ${workers} ended with nonzero non-durable queue depths: ${JSON.stringify(queueClass.nonzero)}.`,
      );
      stage.timingEvidence = summarizeEventTiming(
        stage.eventEvidence,
        stage.providerStdinJournal.entries,
      );
      stage.timingEvidence.providerThreadReplyMs =
        summarizeProviderThreadReplies(
          stage.providerStdinJournal.entries,
          stage.providerStdinJournal.replies,
        );
      if (stage.timingEvidence.readyToProviderStdinMs.state !== "measured")
        journal.metricsGaps.push(
          `Stage ${workers}: exact-event ready-to-stdin p95 is incomplete; every exact input event needs one PID-, boot-, and stage-bounded provider stdin journal witness correlated to its execution attempt.`,
        );
      if (
        stage.timingEvidence.readyToProviderStdinMs.state === "measured" &&
        (stage.timingEvidence.readyToProviderStdinMs.samples.p95Ms ?? 0) >
          stage.timingEvidence.readyToProviderStdinMs.requiredP95LimitMs
      )
        throw new Error(
          `Stage ${workers} ready-to-provider-stdin p95 exceeded ${stage.timingEvidence.readyToProviderStdinMs.requiredP95LimitMs} ms.`,
        );
      if (stage.eventEvidence.providerErrorEvents.length)
        throw new Error(
          `Stage ${workers} has ${stage.eventEvidence.providerErrorEvents.length} provider/event error(s); see exact IDs in evidence.`,
        );
      if (stage.timingEvidence.providerErrors.upstreamLimitMatches.length)
        throw new Error(
          `Stage ${workers} has explicit upstream rate-limit evidence; see exact IDs in evidence.`,
        );
      stage.state = "completed";
      stage.elapsedMs = Math.round(performance.now() - stageStarted);
      stage.finishedAt = new Date().toISOString();
      stage.apiLatencyMs = {
        create: summary(stage.createLatenciesMs),
        message: summary(stage.secondExchangeLatenciesMs),
      };
      stage.apiLatencyPass =
        Number.isFinite(stage.apiLatencyMs.create.p95Ms) &&
        Number.isFinite(stage.apiLatencyMs.message.p95Ms) &&
        (stage.apiLatencyMs.create.p95Ms ?? 0) <= API_P95_LIMIT_MS &&
        (stage.apiLatencyMs.message.p95Ms ?? 0) <= API_P95_LIMIT_MS;
      stage.providerStdinLatencyPass =
        stage.timingEvidence.readyToProviderStdinMs.state === "measured" &&
        (stage.timingEvidence.readyToProviderStdinMs.samples.p95Ms ?? 0) <=
          stage.timingEvidence.readyToProviderStdinMs.requiredP95LimitMs;
      stage.completeTurnDeliveryLatencyMs = summary(
        stage.turnIntervals
          .filter((interval) => Number.isFinite(interval.endedAtMs))
          .map(
            (interval) => (interval.endedAtMs ?? NaN) - interval.startedAtMs,
          ),
      );
      await saveJournal(journalPath, journal);
      process.stdout.write(
        `${JSON.stringify({ stage: workers, state: stage.state, elapsedMs: stage.elapsedMs, actualConcurrentTurns: stage.maxConcurrentTurns, createP95Ms: stage.apiLatencyMs.create.p95Ms, messageP95Ms: stage.apiLatencyMs.message.p95Ms })}\n`,
      );
      if (stage.elapsedMs > STAGE_DEADLINE_MS)
        throw new Error(`Stage ${workers} exceeded its five-minute deadline.`);
      if (!stage.apiLatencyPass)
        throw new Error(
          `Stage ${workers} create/message API p95 exceeded ${API_P95_LIMIT_MS} ms.`,
        );
      // Retire this stage's workers now so later stages start from a quiet
      // backend. The deadline is the same bound the final cleanup uses.
      stage.stageCleanupStartedAt = new Date().toISOString();
      await cleanupOwned(
        client,
        journal,
        journalPath,
        CLEANUP_DEADLINE_MS,
        stream,
        stage,
      );
      stage.stageCleanupFinishedAt = new Date().toISOString();
      await saveJournal(journalPath, journal);
      client.setDeadline(null);
    }
  } catch (error) {
    resultError = error;
    killBarrierProcesses();
    // The failed stage's state is captured before cleanup changes it.
    const failedStage = journal.stages.at(-1);
    if (failedStage && !failedStage.diagnosticsAfter)
      failedStage.diagnosticsAfter = await diagnostics(client).catch(
        (diagnosticError) => ({
          error: String(
            diagnosticError instanceof Error
              ? diagnosticError.message
              : undefined,
          ),
        }),
      );
    journal.result = {
      state: "failed",
      error: String(
        (error instanceof Error ? error.message : undefined) || error,
      ),
      at: new Date().toISOString(),
    };
  } finally {
    client.setDeadline(null);
    client.cleanupMode = true;
    const cleanupStarted = performance.now();
    const cleanupDeadline = cleanupStarted + CLEANUP_DEADLINE_MS;
    client.setDeadline(cleanupDeadline);
    journal.cleanup.state = "running";
    journal.cleanup.startedAt = new Date().toISOString();
    try {
      await saveJournal(journalPath, journal).catch((error) => {
        journal.cleanup.evidenceError = String(
          (error instanceof Error ? error.message : undefined) || error,
        );
      });
      readinessWatch?.close();
      readinessWatch = null;
      if (
        stream &&
        (resultError || interruptRequested || !stream.isHealthy())
      ) {
        await stream.close();
        stream = null;
      }
      if (!stream && journal.ownedAgents?.length)
        stream = await startSyncResourceStream(
          client,
          journal,
          journalPath,
          false,
        );
      await cleanupOwned(
        client,
        journal,
        journalPath,
        Math.max(0, cleanupDeadline - performance.now()),
        stream,
      );
      if (journal.diskPreflight && previousDiskSnapshot) {
        const snapshot = await readFilesystemSnapshot(project);
        assert.ok(
          snapshot.availableBytes >= FREE_DISK_FLOOR_BYTES,
          `Free disk space fell below the ${FREE_DISK_FLOOR_BYTES}-byte floor after cleanup.`,
        );
        journal.cleanup.filesystemDisk = {
          at: snapshot.at,
          availableBytesBefore: previousDiskSnapshot.availableBytes,
          availableBytesAfter: snapshot.availableBytes,
          usedDeltaBytes: snapshot.usedBytes - previousDiskSnapshot.usedBytes,
          deltaAttribution:
            "shared-filesystem delta; not attributed solely to owned worktrees",
        };
      }
      journal.cleanup.elapsedMs = Math.round(
        performance.now() - cleanupStarted,
      );
      journal.cleanup.state = "verified";
      assert.equal(
        barrierProcesses.size,
        0,
        "Owned FIFO readers or release helpers remain active after cleanup.",
      );
    } catch (error) {
      journal.cleanup.state = "incomplete";
      journal.cleanup.error = String(
        (error instanceof Error ? error.message : undefined) || error,
      );
      resultError ||= error;
    }
    if (stream) await stream.close();
    if (journal.cleanup.state === "verified") {
      client.setDeadline(null);
      journal.diagnosticsAfterCleanup = await diagnostics(client);
    }
    journal.metrics = aggregateMetrics(journal);
    const missingStdinTiming = (journal.stages || []).some(
      (stage) =>
        stage.timingEvidence?.readyToProviderStdinMs?.state !== "measured",
    );
    const stopLatencyGap = (journal.stages || []).some(
      (stage) =>
        stage.workerStopApiLatencyMs?.samples !==
          (stage.agentIds || []).length ||
        !Number.isFinite(stage.workerStopApiLatencyMs?.p95Ms),
    );
    const stopLatencyExceeded = (journal.stages || []).some(
      (stage) =>
        Number.isFinite(stage.workerStopApiLatencyMs?.p95Ms) &&
        (stage.workerStopApiLatencyMs?.p95Ms ?? 0) > API_P95_LIMIT_MS,
    );
    const sqliteLockErrorsGap = (journal.stages || []).some(
      (stage) => !Number.isSafeInteger(stage.sqliteLockErrorDelta),
    );
    const queueDrainGap = (journal.stages || []).some(
      (stage) => stage.queueDrain?.state !== "drained",
    );
    const incompleteAcceptance =
      missingStdinTiming ||
      stopLatencyGap ||
      sqliteLockErrorsGap ||
      queueDrainGap ||
      (journal.stages || []).some(
        (stage) => !stage.apiLatencyPass || !stage.providerStdinLatencyPass,
      );
    const overRequestBudget =
      journal.http.length > journal.runBudget.apiRequestsMax;
    journal.result = journal.result || {
      state:
        journal.cleanup.state !== "verified"
          ? "failed"
          : stopLatencyExceeded
            ? "failed"
            : incompleteAcceptance || overRequestBudget
              ? "incomplete_metrics"
              : "passed",
      at: new Date().toISOString(),
      ...(journal.cleanup.state !== "verified"
        ? { reason: "Cleanup did not verify every owned agent as absent." }
        : stopLatencyExceeded
          ? {
              reason: `Per-worker stop API p95 exceeded ${API_P95_LIMIT_MS} ms after cleanup.`,
            }
          : incompleteAcceptance || overRequestBudget
            ? {
                reason: overRequestBudget
                  ? "The HTTP request budget was exceeded."
                  : "One or more S1 latency acceptance metrics could not be established.",
              }
            : {}),
    };
    await saveJournal(journalPath, journal);
    await releaseLock();
    process.stdout.write(
      `${JSON.stringify({ result: journal.result.state, cleanup: journal.cleanup.state, evidence: journalPath, metricsGaps: journal.metricsGaps.length })}\n`,
    );
  }
  if (resultError) throw resultError;
  if (journal.result?.state !== "passed")
    throw new Error(
      `Live workload completed but acceptance evidence is incomplete; see ${journalPath}.`,
    );
}

async function assertEvidenceOutsideCheckout(evidenceDir: string) {
  const [repoRoot, actualEvidenceDir] = await Promise.all([
    realpath(REPO_ROOT),
    realpath(evidenceDir),
  ]);
  const pathFromRepo = relative(repoRoot, actualEvidenceDir);
  assert.ok(
    pathFromRepo === ".." ||
      pathFromRepo.startsWith(`..${sep}`) ||
      isAbsolute(pathFromRepo),
    "Evidence and barrier files must be outside the repository checkout.",
  );
}

function makeStage(workers: number): Stage {
  return {
    workers,
    state: "pending",
    agentIds: [],
    turnIntervals: [],
    maxConcurrentTurns: 0,
    createLatenciesMs: [],
    secondExchangeLatenciesMs: [],
  };
}

async function createClient(
  origin: string,
  journal: Journal,
  journalPath: string,
) {
  const sessionStartedAt = performance.now();
  let pendingRequests = 0;
  const session = await requestJson(
    `${origin}/api/session`,
    { method: "GET" },
    REQUEST_DEADLINE_MS,
  );
  journal.http.push({
    at: new Date().toISOString(),
    method: "GET",
    path: "/api/session",
    status: 200,
    elapsedMs: Math.round(performance.now() - sessionStartedAt),
  });
  await saveJournal(journalPath, journal);
  assert.ok(
    typeof session.token === "string" && session.token.length > 0,
    "Backend did not provide its session token.",
  );
  const headers = {
    "content-type": "application/json",
    origin,
    "x-canvas-token": session.token,
  };
  const request = async <T extends object = Schema["StopResponse"]>(
    method: string,
    path: string,
    body: unknown = undefined,
    deadlineMs = REQUEST_DEADLINE_MS,
  ) => {
    const remainingStage = client.stageDeadlineAt
      ? client.stageDeadlineAt - performance.now()
      : Infinity;
    const effectiveDeadline = Math.min(deadlineMs, remainingStage);
    if (effectiveDeadline <= 0)
      throw new Error("Stage deadline elapsed before the request began.");
    const releaseRequest = client.reserveApiRequest();
    const timing = deadlineSignal(
      effectiveDeadline,
      client.cleanupMode ? null : workloadAbort.signal,
    );
    const startedAt = performance.now();
    const at = new Date().toISOString();
    try {
      const response = await fetch(new URL(path, origin), {
        method,
        headers,
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
        signal: timing.signal,
      });
      const text = await response.text();
      const elapsedMs = Math.round(performance.now() - startedAt);
      let value: unknown;
      try {
        value = text ? JSON.parse(text) : null;
      } catch {
        value = { responseText: text.slice(0, 1000) };
      }
      const record = { at, method, path, status: response.status, elapsedMs };
      journal.http.push(record);
      await saveJournal(journalPath, journal);
      if (!response.ok)
        throw Object.assign(
          new Error(
            `${method} ${path} returned HTTP ${response.status}: ${JSON.stringify(value).slice(0, 500)}`,
          ),
          { status: response.status, body: value, elapsedMs },
        );
      assert.ok(value !== null && typeof value === "object");
      return { value: value as T, elapsedMs, at };
    } catch (error) {
      if (
        !(
          typeof error === "object" &&
          error !== null &&
          "status" in error &&
          error.status
        )
      ) {
        const record = {
          at,
          method,
          path,
          status: null,
          elapsedMs: Math.round(performance.now() - startedAt),
          outcome: "unknown",
          error: String(
            (error instanceof Error ? error.message : undefined) || error,
          ).slice(0, 500),
        };
        journal.http.push(record);
        await saveJournal(journalPath, journal);
        throw Object.assign(
          new Error(
            `${method} ${path} outcome is unknown; inspect the exact identity before any later action. ${record.error}`,
          ),
          { unknown: true },
        );
      }
      throw error;
    } finally {
      timing.close();
      releaseRequest();
    }
  };
  const get = async <T extends object>(path: string): Promise<T> =>
    (await request<T>("GET", path)).value;
  const write = async (
    method: string,
    path: string,
    body: Schema["CreateAgentRequest"] | Schema["ManagedMessageBody"],
    identity: {
      type: "create" | "message";
      id: string;
      parentId?: string;
      isLead?: boolean;
      agentId?: string;
      stageWorkers: number;
      stageIndex?: number;
    },
  ) => {
    const intent: Intent = {
      ...identity,
      method,
      path,
      requestId: body.id,
      state: "prepared",
      preparedAt: new Date().toISOString(),
    };
    journal.ownedAgents ||= [];
    if (identity.type === "create") {
      const owner: OwnedAgent = {
        id: identity.id,
        parentId: identity.parentId || null,
        isLead: identity.isLead === true,
        stageWorkers: identity.stageWorkers,
        ...(identity.stageIndex === undefined
          ? {}
          : { stageIndex: identity.stageIndex }),
        create: intent,
        exchanges: [],
        cleanup: { state: "pending" },
      };
      journal.ownedAgents.push(owner);
      await saveJournal(journalPath, journal);
    } else if (identity.type === "message") {
      const owner = journal.ownedAgents.find(
        (item) => item.id === identity.agentId,
      );
      assert.ok(
        owner,
        `Message owner is not in the durable journal: ${identity.agentId}`,
      );
      owner.exchanges.push(intent);
      await saveJournal(journalPath, journal);
    }
    intent.state = "sending";
    await saveJournal(journalPath, journal);
    let result;
    try {
      result = await request<
        | Schema["AgentResponse"]
        | Schema["studio_api__work__models__MessageReceipt"]
      >(method, path, body);
    } catch (error) {
      intent.state =
        typeof error === "object" &&
        error !== null &&
        "unknown" in error &&
        error.unknown
          ? "unknown"
          : "failed";
      intent.error = String(
        (error instanceof Error ? error.message : undefined) || error,
      ).slice(0, 500);
      await saveJournal(journalPath, journal);
      throw error;
    }
    intent.state = "confirmed";
    intent.responseAt = result.at;
    intent.elapsedMs = result.elapsedMs;
    intent.receipt =
      "status" in result.value && typeof result.value.status === "string"
        ? result.value.status
        : null;
    await saveJournal(journalPath, journal);
    return { ...result, elapsedMs: result.elapsedMs };
  };
  const client = {
    origin,
    observedAgentIds: new Set(
      (journal.ownedAgents || []).map((owner) => owner.id),
    ),
    get,
    write,
    request,
    stageDeadlineAt: null as number | null,
    cleanupMode: false,
    reserveApiRequest() {
      const limit = this.cleanupMode
        ? API_REQUESTS_MAX
        : API_REQUESTS_MAX - CLEANUP_API_REQUEST_RESERVE;
      assert.ok(
        journal.http.length + pendingRequests < limit,
        `HTTP request budget reached its ${limit}-request ${this.cleanupMode ? "total" : "pre-cleanup"} cap.`,
      );
      pendingRequests += 1;
      return () => {
        pendingRequests -= 1;
      };
    },
    setDeadline(deadlineAt: number | null) {
      this.stageDeadlineAt = deadlineAt;
    },
    async agentSnapshot(id: string) {
      const entity = await this.rawAgentSnapshot(id);
      return entity?.deletedAt ? null : entity;
    },
    async rawAgentSnapshot(id: string) {
      const url = new URL("/api/sync/pull", origin);
      url.searchParams.set("scope", "state:entities:v1");
      url.searchParams.set("after", "0");
      url.searchParams.set("fresh", "1");
      url.searchParams.set("reset", "1");
      url.searchParams.set("limit", "1");
      url.searchParams.set("priorityId", id);
      const result = (
        await request<Schema["SyncPullResponse"]>(
          "GET",
          `${url.pathname}${url.search}`,
        )
      ).value;
      const document = (result.documents || []).find(
        (item) => parseStateEntity(item)?.id === id,
      );
      if (!document || ("_deleted" in document && document._deleted))
        return null;
      return parseStateEntity(document);
    },
    async receipt(agentId: string, messageId: string) {
      const url = new URL("/api/messages/receipts", origin);
      url.searchParams.set("agent", agentId);
      url.searchParams.set("ids", JSON.stringify([messageId]));
      const result = await request<Schema["MessageReceipts"]>(
        "GET",
        `${url.pathname}${url.search}`,
      );
      return result.value?.items?.find((item) => item.id === messageId) || null;
    },
  };

  return client;
}

async function requestJson(
  url: string,
  init: RequestInit,
  timeoutMs: number,
): Promise<Schema["SessionResponse"]> {
  const timing = deadlineSignal(timeoutMs);
  try {
    const response = await fetch(url, { ...init, signal: timing.signal });
    const body = await response.json();
    assert.ok(response.ok, `GET ${url} returned HTTP ${response.status}.`);
    return body;
  } finally {
    timing.close();
  }
}

async function startSyncResourceStream(
  client: Client,
  journal: Journal,
  journalPath: string,
  interruptible = true,
  resources: Schema["ResourceRef"][] = [{ kind: "state" }],
) {
  assert.ok(journal.backendIdentity && journal.ownerBaseline);

  const resource = encodeURIComponent(JSON.stringify(resources));
  const url = new URL(
    `/api/sync/stream?protocol=3&resources=${resource}`,
    client.origin,
  );
  const token = (await client.get<Schema["SessionResponse"]>("/api/session"))
    .token;
  const connectDeadlineMs = client.stageDeadlineAt
    ? Math.min(REQUEST_DEADLINE_MS, client.stageDeadlineAt - performance.now())
    : REQUEST_DEADLINE_MS;
  assert.ok(
    connectDeadlineMs > 0,
    "Deadline elapsed before opening the state event stream.",
  );
  const timing = deadlineSignal(
    connectDeadlineMs,
    interruptible ? workloadAbort.signal : null,
  );
  const startedAt = performance.now();
  let response;
  let keepSignal = false;
  let releaseApiBudget: (() => void) | null = client.reserveApiRequest();
  try {
    response = await fetch(url, {
      headers: {
        origin: client.origin,
        "x-canvas-token": token,
        "X-Codex-Sync-Protocol": "3",
      },
      signal: timing.signal,
    });
    journal.http.push({
      at: new Date().toISOString(),
      method: "GET",
      path: "/api/sync/stream",
      status: response.status,
      elapsedMs: Math.round(performance.now() - startedAt),
      eventStream: true,
    });
    await saveJournal(journalPath, journal);
    releaseApiBudget();
    releaseApiBudget = null;
    assert.ok(
      response.ok && response.body,
      `State event stream failed with HTTP ${response.status}.`,
    );
    timing.disarm();
    keepSignal = true;
  } catch (error) {
    journal.http.push({
      at: new Date().toISOString(),
      method: "GET",
      path: "/api/sync/stream",
      status: null,
      elapsedMs: Math.round(performance.now() - startedAt),
      eventStream: true,
      outcome: "unknown",
      error: String(
        (error instanceof Error ? error.message : undefined) || error,
      ).slice(0, 500),
    });
    await saveJournal(journalPath, journal);
    throw error;
  } finally {
    releaseApiBudget?.();
    if (!keepSignal) timing.close();
  }
  const events = createBoundedEventQueue<StreamEvent>(10_000);
  const stateResourceIndex = resources.findIndex(
    (resource) => resource.kind === "state",
  );
  let stateSequence: number | null = null;
  let pendingStateSequence: number | null = null;
  let observeStateResource:
    | ReturnType<typeof createStateResourceAdapter>
    | undefined;
  let failure: unknown;
  let reader;
  try {
    reader = response.body.getReader();
  } catch (error) {
    await response.body.cancel().catch(() => {});
    timing.close();
    throw error;
  }
  const decoder = new TextDecoder();
  let buffer = "";
  let event: { data: string[]; name?: string } = { data: [] };
  let wake: (() => void) | undefined;
  const pushEvent = (value: StreamEvent) => {
    if (value.name === "resources") {
      const version = stateResourceVersion(value.value);
      if (version && Number.isSafeInteger(version.revision))
        pendingStateSequence = Math.max(
          pendingStateSequence ?? version.revision,
          version.revision,
        );
    }
    events.push(value);
    if (events.failure) failure = events.failure;
    wake?.();
    wake = undefined;
  };
  const pump = (async () => {
    try {
      while (true) {
        const chunk = await reader.read();
        if (chunk.done)
          throw new Error("State sync event stream ended unexpectedly.");
        buffer += decoder.decode(chunk.value, { stream: true });
        let boundary;
        while ((boundary = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, boundary).replace(/\r$/, "");
          buffer = buffer.slice(boundary + 1);
          if (!line) {
            if (event.data.length) {
              try {
                pushEvent({
                  name: event.name || "message",
                  value: JSON.parse(event.data.join("\n")),
                  receivedAt: performance.now(),
                });
              } catch (error) {
                failure = error;
                wake?.();
                wake = undefined;
              }
            }
            event = { data: [] };
          } else if (line.startsWith("event:"))
            event.name = line.slice(6).trim();
          else if (line.startsWith("data:"))
            event.data.push(line.slice(5).trimStart());
        }
      }
    } catch (error) {
      failure = error;
      wake?.();
    }
  })();
  // Wakes a pending `next` with an "interrupted" event so an observer can stop
  // without leaving an outstanding read behind. The observer is always running
  // when this is called, so the flag is consumed by its next read.
  let interrupted = false;
  const nextRaw = async (timeoutAt: number): Promise<StreamEvent> => {
    while (!events.length) {
      if (failure) throw failure;
      if (interrupted) {
        interrupted = false;
        return { name: "interrupted" };
      }
      const remaining = timeoutAt - performance.now();
      if (Number.isFinite(timeoutAt) && remaining <= 0)
        throw new Error("Timed out waiting for an authoritative state event.");
      await new Promise<void>((resolvePromise, rejectPromise) => {
        const timer = Number.isFinite(timeoutAt)
          ? setTimeout(() => {
              wake = undefined;
              rejectPromise(
                new Error(
                  "Timed out waiting for an authoritative state event.",
                ),
              );
            }, remaining)
          : null;
        wake = () => {
          if (timer) clearTimeout(timer);
          resolvePromise();
        };
      });
    }
    if (failure) throw failure;
    const event = events.shift();
    assert.ok(event);
    return event;
  };
  const next = async (timeoutAt: number) => {
    const received = await nextRaw(timeoutAt);
    if (stateResourceIndex < 0 || received.name !== "resources")
      return received;
    const version = stateResourceVersion(received.value);
    if (!version || !Number.isSafeInteger(version.revision))
      throw new Error("State SSE frame omitted its entity-sequence revision.");
    if (!observeStateResource) {
      observeStateResource = createStateResourceAdapter(
        journal.ownerBaseline?.workspaceId ?? "",
        client.observedAgentIds,
        async (after) => {
          const url = new URL("/api/sync/pull", client.origin);
          url.searchParams.set("scope", "state:entities:v1");
          url.searchParams.set("after", String(after));
          url.searchParams.set("reset", "1");
          return client.get<EntityPage>(`${url.pathname}${url.search}`);
        },
      );
    }
    assert.ok(received.value);
    const observation = await observeStateResource(
      received.value,
      pendingStateSequence,
    );
    stateSequence = observation.sequence;
    received.stateDocuments = observation.stateDocuments;
    if (pendingStateSequence !== null && pendingStateSequence <= stateSequence)
      pendingStateSequence = null;
    if (received.stateDocuments.length) received.receivedAt = performance.now();
    return received;
  };
  return {
    next,
    events,
    interrupt() {
      interrupted = true;
      wake?.();
    },
    async close() {
      await reader.cancel().catch(() => {});
      timing.close();
      await Promise.race([
        pump,
        new Promise<void>((resolvePromise) => setTimeout(resolvePromise, 500)),
      ]);
    },
    isHealthy() {
      return !failure;
    },
  };
}

// Records worker turn intervals from state events until the stream is
// interrupted. The intervals are evidence; they are not the concurrency gate.
async function observeWorkerTurns(
  stream: SyncStream,
  ids: string[],
  deadlineAt: number,
  stage: Stage,
  journal: Journal,
  journalPath: string,
) {
  const expected = new Set(ids);
  assert.ok(stage.leadId);
  const monitored = new Set([...ids, stage.leadId]);
  const active = new Map<string, string>();
  let max = 0;
  while (performance.now() < deadlineAt) {
    const event = await stream.next(deadlineAt);
    if (event.name === "interrupted") return max;
    if (event.name !== "resources") continue;
    for (const document of event.stateDocuments || []) {
      const entity = parseStateEntity(document);
      if (!entity || !entity.id || !monitored.has(entity.id)) continue;
      const at = event.receivedAt;
      assert.ok(at !== undefined);
      if (entity.id === stage.leadId) {
        const open = stage.turnIntervals.findLast(
          (row) => row.agentId === stage.leadId && row.endedAtMs === undefined,
        );
        if (
          entity.inFlight &&
          typeof entity.turnId === "string" &&
          entity.turnId
        ) {
          if (!open || open.turnId !== entity.turnId) {
            if (open) open.endedAtMs = at;
            stage.turnIntervals.push({
              agentId: stage.leadId,
              turnId: entity.turnId,
              startedAtMs: at,
              observedAt: new Date().toISOString(),
            });
          }
        } else if (open) {
          open.endedAtMs = at;
        }
        await saveJournal(journalPath, journal);
        continue;
      }
      if (
        entity.inFlight &&
        typeof entity.turnId === "string" &&
        entity.turnId
      ) {
        if (!active.has(entity.id)) {
          active.set(entity.id, entity.turnId);
          assert.equal(
            new Set(active.values()).size,
            active.size,
            "Workers exposed the same native turn ID during overlap proof.",
          );
          stage.turnIntervals.push({
            agentId: entity.id,
            turnId: entity.turnId,
            startedAtMs: at,
            observedAt: new Date().toISOString(),
          });
          max = Math.max(max, active.size);
          stage.maxConcurrentTurns = max;
          if (active.size === expected.size && !stage.allWorkersActiveAt)
            stage.allWorkersActiveAt = new Date().toISOString();
        }
      } else if (active.has(entity.id)) {
        const turnId = active.get(entity.id);
        const interval = stage.turnIntervals.findLast(
          (row) =>
            row.agentId === entity.id &&
            row.turnId === turnId &&
            row.endedAtMs === undefined,
        );
        if (interval) interval.endedAtMs = at;
        active.delete(entity.id);
      }
    }
    await saveJournal(journalPath, journal);
  }
  return max;
}

async function waitForStageSettled(
  stream: SyncStream,
  ids: string[],
  deadlineAt: number,
  stage: Stage,
  journal: Journal,
  journalPath: string,
) {
  const expected = new Set(ids);
  const state = new Map<string, Schema["AgentEntityDto"]>();
  const isSettled = (entity: Schema["AgentEntityDto"]) =>
    !entity.inFlight &&
    ["completed", "failed", "interrupted", "paused"].some(
      (status) => status === entity.status,
    );
  while (
    (state.size < expected.size ||
      [...state.values()].some((entity) => !isSettled(entity))) &&
    performance.now() < deadlineAt
  ) {
    const event = await stream.next(deadlineAt);
    if (event.name !== "resources") continue;
    for (const document of event.stateDocuments || []) {
      const entity = parseStateEntity(document);
      if (!entity || !entity.id || !expected.has(entity.id)) continue;
      state.set(entity.id, entity);
      if (
        entity.inFlight &&
        typeof entity.turnId === "string" &&
        entity.turnId
      ) {
        const known = stage.turnIntervals.some(
          (row) =>
            row.agentId === entity.id &&
            row.turnId === entity.turnId &&
            row.endedAtMs === undefined,
        );
        if (!known) {
          stage.turnIntervals.push({
            agentId: entity.id,
            turnId: entity.turnId,
            startedAtMs: event.receivedAt ?? performance.now(),
            observedAt: new Date().toISOString(),
          });
          stage.maxConcurrentTurns = Math.max(
            stage.maxConcurrentTurns,
            stage.turnIntervals.filter((row) => row.endedAtMs === undefined)
              .length,
          );
        }
      }
      if (isSettled(entity)) {
        const interval = stage.turnIntervals.findLast(
          (row) =>
            row.agentId === entity.id &&
            (!entity.turnId || row.turnId === entity.turnId) &&
            row.endedAtMs === undefined,
        );
        if (interval)
          interval.endedAtMs = event.receivedAt ?? performance.now();
      }
    }
    stage.settledCount = [...state.values()].filter(isSettled).length;
    await saveJournal(journalPath, journal);
    if (
      [...state.values()].some(
        (entity) =>
          entity.status === "failed" ||
          entity.status === "interrupted" ||
          entity.status === "paused",
      )
    )
      throw new Error(
        `Stage ${stage.workers} has a failed/interrupted/paused provider turn.`,
      );
  }
  if (
    state.size < expected.size ||
    [...state.values()].some((entity) => !isSettled(entity))
  )
    throw new Error(
      `Stage ${stage.workers} did not settle every worker before the five-minute deadline.`,
    );
  if (
    [...state.values()].some(
      (entity) => entity.inFlight || entity.status !== "completed",
    )
  )
    throw new Error(
      `Stage ${stage.workers} did not complete every provider turn.`,
    );
}

async function verifyToolCallback(
  client: Client,
  agentId: string,
  runId: string,
  workers: number,
  index: number,
) {
  const url = new URL("/api/transcript/page", client.origin);
  url.searchParams.set("id", agentId);
  const page = await client.get<Schema["TranscriptPageResponse"]>(
    `${url.pathname}${url.search}`,
  );
  const rows = page.items || [];
  const tool = rows.some(
    (row) =>
      /tool|command/i.test(
        `${row.kind || ""} ${row.phase || ""} ${row.title || ""}`,
      ) && /STUDIO_SCALE_|printf/i.test(row.text || ""),
  );
  assert.ok(
    tool,
    `Stage ${workers} tool witness ${agentId} has no persisted provider tool callback.`,
  );
  const expectedOutput = `STUDIO_SCALE_${runId.slice(0, 8)}_${workers}_${index}_TOOL_OK`;
  assert.equal(
    rows.filter((row) => row.toolStatus === "running").length,
    0,
    `Stage ${workers} worker ${agentId} still has a running tool callback.`,
  );
  const successfulCallbacks = rows.filter((row) => {
    if (
      row.toolStatus !== "completed" ||
      !/tool|command/i.test(
        `${row.kind || ""} ${row.phase || ""} ${row.title || ""}`,
      )
    )
      return false;
    try {
      const record: Record<string, unknown> = JSON.parse(row.text || "{}");
      return (
        typeof record.aggregatedOutput === "string" &&
        record.aggregatedOutput.includes(expectedOutput)
      );
    } catch {
      return false;
    }
  });
  assert.equal(
    successfulCallbacks.length,
    1,
    `Stage ${workers} callback ${agentId} must persist exactly one successful shell callback.`,
  );
  assert.ok(
    successfulCallbacks[0],
    `Stage ${workers} callback ${agentId} did not persist the expected successful shell output.`,
  );
  const replies = rows.filter((row) =>
    /assistant|agent/i.test(
      `${row.kind || ""} ${row.phase || ""} ${row.role || ""}`,
    ),
  );
  const firstTag = `STUDIO_SCALE_${runId.slice(0, 8)}_${workers}_${index}`;
  const secondTag = `STUDIO_SCALE_SECOND_${runId.slice(0, 8)}_${workers}_${index}`;
  for (const tag of [firstTag, secondTag]) {
    const count = replies.filter(
      (row) => String(row.text || "").trim() === tag,
    ).length;
    assert.equal(
      count,
      1,
      `Stage ${workers} worker ${agentId} must persist exactly one assistant reply tagged ${tag}; found ${count}.`,
    );
  }
  const providerLimit = rows.filter((row) =>
    showsUpstreamLimit(
      `${row.text || ""} ${JSON.stringify(row.turnError || "")}`,
    ),
  );
  if (providerLimit.length)
    throw new Error(
      `Stage ${workers} tool witness exposed upstream limit/error evidence.`,
    );
  return {
    agentId,
    persistedToolTranscriptRows: rows.filter((row) =>
      /tool|command/i.test(
        `${row.kind || ""} ${row.phase || ""} ${row.title || ""}`,
      ),
    ).length,
    successfulOutput: expectedOutput,
    callbackProcessesRetired: true,
    verifiedAt: new Date().toISOString(),
    runId,
  };
}

async function diagnostics(client: Client) {
  try {
    const value =
      await client.get<Schema["DiagnosticsResponse"]>("/api/diagnostics");
    return {
      at: new Date().toISOString(),
      httpRequestMetrics: value.httpRequestMetrics || null,
      runtimeLockOperations: value.runtimeLockOperations || null,
      sqliteContention: value.sqliteContention || null,
      queues: value.queues || null,
      processTree: value.processTree || null,
      blockingPoolMetrics: value.blockingPoolMetrics || null,
      upstreamLimits:
        "only exposed errors/events are attributable; no provider-specific aggregate field found",
    };
  } catch (error) {
    return {
      at: new Date().toISOString(),
      unavailable: String(
        (error instanceof Error ? error.message : undefined) || error,
      ),
    };
  }
}

function observedSqliteLockErrors(
  value: DiagnosticSnapshot | null | undefined,
) {
  const count = value?.sqliteContention?.lockError?.lockErrors;
  return typeof count === "number" && Number.isSafeInteger(count) && count >= 0
    ? count
    : null;
}

// Every evidence reader shares one read-only transaction and workspace fence.
function withEvidenceDatabase<T>(
  stateDir: string,
  workspaceId: string,
  deadlineAt: number,
  read: (database: DatabaseSync) => T,
) {
  const remainingMs = Math.floor(
    Math.min(REQUEST_DEADLINE_MS, deadlineAt - performance.now()),
  );
  assert.ok(remainingMs > 0, "SQLite evidence deadline expired");
  const database = new DatabaseSync(join(stateDir, "canvas.sqlite3"), {
    readOnly: true,
    timeout: remainingMs,
  });
  try {
    database.exec("PRAGMA query_only=ON; BEGIN");
    assert.equal(
      database.prepare("SELECT id FROM sync_identity").get()?.id,
      workspaceId,
      "SQLite workspace identity changed",
    );
    const result = read(database);
    assert.ok(
      performance.now() < deadlineAt,
      "SQLite evidence deadline expired",
    );
    return result;
  } finally {
    database.close();
  }
}

export function readServedModelRecords(
  stateDir: string,
  workspaceId: string,
  agentIds: string[],
  deadlineAt: number,
  usageSource: string | null = "claudeResponse",
) {
  return withEvidenceDatabase(stateDir, workspaceId, deadlineAt, (database) => {
    database
      .prepare("ATTACH DATABASE ? AS analytics")
      .run(
        pathToFileURL(join(resolve(stateDir), "analytics.sqlite3")).href +
          "?mode=ro",
      );
    const placeholders = agentIds.map(() => "?").join(",");
    // Agent ids are scoped by the caller's provider profile. Typed response
    // evidence is provider-neutral; only pre-contract rows use usageSource.
    const rows = database
      .prepare(
        "SELECT agent AS agentId,turn AS turnId," +
          "CASE WHEN json_extract(record,'$.studioUsage.kind')='response' THEN json_extract(record,'$.studioUsage.model') ELSE json_extract(record,'$.model') END AS model," +
          "CASE WHEN json_type(record,'$.studioUsage') IS NOT NULL THEN json_extract(record,'$.studioUsage.responseId') ELSE json_extract(record,'$.responseId') END AS responseId " +
          "FROM analytics.analytics_usage WHERE agent IN (" +
          placeholders +
          ") AND CASE WHEN json_type(record,'$.studioUsage') IS NOT NULL THEN " +
          (usageSource
            ? "json_extract(record,'$.studioUsage.kind')='response'"
            : "(json_extract(record,'$.studioUsage.kind')='response' OR (json_extract(record,'$.studioUsage.kind')='cumulative' AND json_extract(record,'$.studioUsage.scope')='codexThread'))") +
          " ELSE COALESCE(json_extract(record,'$.usageSource'),'')" +
          (usageSource ? "=" : "!=") +
          "? END ORDER BY seq",
      )
      .all(...agentIds, usageSource || "claudeResponse");
    const records = new Map<string, ServedModelRecord[]>();
    for (const { agentId, ...row } of rows) {
      assert.ok(typeof agentId === "string");
      assert.ok(typeof row.model === "string" || row.model === null);
      assert.ok(typeof row.turnId === "string" || row.turnId === null);
      assert.ok(typeof row.responseId === "string" || row.responseId === null);
      const list = records.get(agentId) || [];
      list.push({
        model: row.model,
        turnId: row.turnId,
        responseId: row.responseId,
      });
      records.set(agentId, list);
    }
    return records;
  });
}

// Fails closed unless every agent has at least one served Haiku response per
// completed exchange. The served-model source is the bridge's response model
// stored in analytics_usage; no configured model is ever substituted for it.
async function verifyServedModel(
  stream: SyncStream,
  journal: Journal,
  profile: ScaleProfile,
  agentIds: string[],
  minimumResponses: number,
  deadlineAt: number,
) {
  assert.ok(journal.backendIdentity && journal.ownerBaseline);

  const { backendIdentity, ownerBaseline } = journal;
  const records = await waitForServedModelEvidence({
    profile,
    agentIds,
    minimumResponses,
    readRecords: () =>
      readServedModelRecords(
        backendIdentity.stateDir,
        ownerBaseline.workspaceId,
        agentIds,
        deadlineAt,
        profile.usageSource,
      ),
    waitForPublication: async () => {
      while ((await stream.next(deadlineAt)).name !== "resources") {}
    },
  });
  const allRecords = [...records.values()].flat();
  return {
    checkedAt: new Date().toISOString(),
    minimumResponsesPerAgent: minimumResponses,
    agents: agentIds.length,
    responses: allRecords.length,
    models: [...new Set(allRecords.map((record) => record.model))].sort(),
    provenance:
      "analytics_usage record.model; a configured-model fallback is not distinguishable in the stored row",
  };
}

// The one read-only SQLite snapshot for exact identities. Every caller gets the
// workspace fence and every retained input in the same transaction.
export function runEventEvidenceQuery(
  stateDir: string,
  workspaceId: string,
  eventIds: { id: string; agentId: string; exchange: number }[],
  agentIds: string[],
  deadlineAt: number,
) {
  return withEvidenceDatabase(stateDir, workspaceId, deadlineAt, (database) => {
    const events: EvidenceEvent[] = eventIds.map((item) => {
      const row = database
        .prepare(
          "SELECT e.id,e.agent AS agentId,e.kind,e.status,e.error,m.record FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=? AND e.agent=?",
        )
        .get(item.id, item.agentId);
      if (!row)
        return {
          id: item.id,
          agentId: item.agentId,
          exchange: item.exchange,
          missing: true,
        };
      assert.ok(row.record === null || typeof row.record === "string");
      assert.ok(
        typeof row.id === "string" &&
          typeof row.agentId === "string" &&
          typeof row.kind === "string" &&
          typeof row.status === "string" &&
          (row.error === null || typeof row.error === "string"),
      );
      const metadata: unknown = row.record ? JSON.parse(row.record) : {};
      const timing =
        typeof metadata === "object" &&
        metadata !== null &&
        "timing" in metadata
          ? metadata.timing
          : undefined;
      const attempts = database
        .prepare(
          "SELECT a.id,json_extract(a.record,'$.requestTicket') AS requestTicket,json_extract(a.record,'$.connectionGeneration') AS connectionGeneration,json_extract(a.record,'$.accountKey') AS accountKey,json_extract(a.record,'$.events') AS events,r.account AS runAccountKey FROM runtime_execution_inputs i JOIN runtime_execution_runs r ON r.id=i.run JOIN runtime_execution_attempts a ON a.run=i.run WHERE i.event=? AND r.agent=? ORDER BY a.id",
        )
        .all(item.id, item.agentId)
        .map((attempt): AttemptEvidence => {
          assert.ok(
            typeof attempt.id === "string" &&
              (attempt.accountKey === null ||
                typeof attempt.accountKey === "string") &&
              (attempt.events === null || typeof attempt.events === "string") &&
              (attempt.requestTicket === null ||
                typeof attempt.requestTicket === "string") &&
              (attempt.connectionGeneration === null ||
                typeof attempt.connectionGeneration === "number") &&
              (attempt.runAccountKey === null ||
                typeof attempt.runAccountKey === "string"),
          );
          return {
            id: attempt.id,
            accountKey: attempt.accountKey,
            requestTicket: attempt.requestTicket,
            connectionGeneration: attempt.connectionGeneration,
            runAccountKey: attempt.runAccountKey,
            events: attempt.events ? JSON.parse(attempt.events) : [],
          };
        });
      return {
        id: row.id,
        agentId: row.agentId,
        kind: row.kind,
        status: row.status,
        exchange: item.exchange,
        error: (row.error || "").slice(0, 500),
        timing:
          typeof timing === "object" &&
          timing !== null &&
          "enqueuedAt" in timing
            ? { enqueuedAt: timing.enqueuedAt }
            : {},
        attempts,
      };
    });
    const retainedInputs = database
      .prepare(
        "SELECT id,agent,kind,status FROM runtime_events WHERE status IN (" +
          RETAINED_INPUT_STATUSES.map(() => "?").join(",") +
          ") ORDER BY id",
      )
      .all(...RETAINED_INPUT_STATUSES)
      .map((row): RetainedInput => {
        assert.ok(
          typeof row.id === "string" &&
            typeof row.agent === "string" &&
            typeof row.kind === "string" &&
            typeof row.status === "string",
        );
        return {
          id: row.id,
          agent: row.agent,
          kind: row.kind,
          status: row.status,
        };
      });
    const nativeTurnCounts = Object.fromEntries(
      agentIds.map((agentId) => {
        const count = database
          .prepare(
            "SELECT count(*) AS count FROM runtime_completed_turns WHERE id>=? AND id<?",
          )
          .get(agentId + ":", agentId + ";")?.count;
        assert.ok(typeof count === "number");
        return [agentId, count];
      }),
    );
    return {
      workspaceId,
      database: resolve(stateDir, "canvas.sqlite3"),
      events,
      nativeTurnCounts,
      retainedInputs,
    };
  });
}

// Retained inputs of every agent, with no exact event requested. The baseline
// and the post-cleanup audit read through this without a stage.
export function readRetainedInputRows(
  stateDir: string,
  workspaceId: string,
  deadlineAt: number,
): RetainedInputRows {
  return runEventEvidenceQuery(stateDir, workspaceId, [], [], deadlineAt)
    .retainedInputs;
}

async function readEventEvidence(
  stateDir: string,
  workspaceId: string,
  stage: Stage,
  deadlineAt: number,
) {
  assert.ok(stage.secondExchangeIds && stage.leadId);
  const secondIds = stage.secondExchangeIds;
  const eventIds = stage.agentIds.flatMap((agentId, index) => [
    { id: `${agentId}:initial`, agentId, exchange: 1 },
    {
      id: secondIds[index] ?? "",
      agentId,
      exchange: 2,
    },
  ]);
  const evidence = runEventEvidenceQuery(
    stateDir,
    workspaceId,
    eventIds,
    [stage.leadId, ...stage.agentIds],
    deadlineAt,
  );
  return {
    at: new Date().toISOString(),
    database: evidence.database,
    nativeTurnCounts: evidence.nativeTurnCounts,
    exactIdsRequested: eventIds.length,
    events: evidence.events,
    missingCount: evidence.events.filter((event) => event.missing).length,
    providerErrorEvents: evidence.events
      .filter((event) => event.error)
      .map(({ id, agentId, exchange, error }) => ({
        id,
        agentId,
        exchange,
        error,
      })),
    retainedInputs: evidence.retainedInputs,
  };
}

export function readProviderStdinJournal(
  backendIdentity: BackendIdentity,
  stage: Pick<Stage, "providerStdinJournalWindow">,
  deadlineAt: number,
) {
  const bootId = canonicalJournalBootId(backendIdentity.bootId);
  const window = stage.providerStdinJournalWindow;
  assert.ok(
    window && Date.parse(window.startedAt) <= Date.parse(window.finishedAt),
    "Stage has no valid provider stdin journal time window.",
  );
  const result = spawnSync(
    "journalctl",
    [
      "--no-pager",
      "--output=json",
      `--boot=${bootId}`,
      `--since=${window.startedAt}`,
      `--until=${window.finishedAt}`,
      `_PID=${backendIdentity.pid}`,
      `_BOOT_ID=${bootId}`,
    ],
    {
      encoding: "utf8",
      timeout: Math.max(
        1,
        Math.floor(
          Math.min(REQUEST_DEADLINE_MS, deadlineAt - performance.now()),
        ),
      ),
      maxBuffer: 16 * 1024 * 1024,
    },
  );
  if (result.error || result.status !== 0)
    throw new Error(
      `Could not read the PID-, boot-, and stage-bounded provider stdin journal: ${String((result.error instanceof Error ? result.error.message : undefined) || result.stderr || result.status).slice(0, 800)}`,
    );
  const startedAtUs = Date.parse(window.startedAt) * 1_000;
  const finishedAtUs = Date.parse(window.finishedAt) * 1_000;
  const entries = [];
  const replies = [];
  for (const line of result.stdout.split("\n")) {
    if (!line) continue;
    const envelope: Record<string, unknown> = JSON.parse(line);
    if (
      envelope._PID !== String(backendIdentity.pid) ||
      envelope._BOOT_ID !== bootId
    )
      throw new Error(
        "Journal returned a row outside the trusted process scope.",
      );
    const journalTimeUs = Number(envelope.__REALTIME_TIMESTAMP);
    if (
      !Number.isSafeInteger(journalTimeUs) ||
      journalTimeUs < startedAtUs ||
      journalTimeUs > finishedAtUs
    )
      throw new Error(
        "Journal returned a row outside the stage wall-clock window.",
      );
    const entry = parseProviderStdinJournalMessage(
      typeof envelope.MESSAGE === "string" ? envelope.MESSAGE : "",
    );
    if (entry) entries.push({ ...entry, journalTimeUs });
    const reply = parseProviderReplyJournalMessage(
      typeof envelope.MESSAGE === "string" ? envelope.MESSAGE : "",
    );
    if (reply) replies.push({ ...reply, journalTimeUs });
  }
  return {
    window,
    pid: backendIdentity.pid,
    bootId,
    entries,
    replies,
  };
}

export async function readCurrentTeamChildren(
  stateDir: string,
  workspaceId: string,
  leadId: string,
  deadlineAt: number,
  agentIds: string[] = [],
) {
  return withEvidenceDatabase(stateDir, workspaceId, deadlineAt, (database) => {
    const rows = agentIds.length
      ? database
          .prepare(
            "SELECT id,record FROM runtime_agents WHERE id IN (" +
              agentIds.map(() => "?").join(",") +
              ") ORDER BY id",
          )
          .all(...agentIds)
      : database
          .prepare(
            "SELECT id,record FROM runtime_agents WHERE id=? OR json_extract(record,'$.rootId')=? ORDER BY id",
          )
          .all(leadId, leadId);
    return rows.map(({ id, record: raw }): TeamChild => {
      assert.ok(typeof id === "string" && typeof raw === "string");
      const record: Schema["AgentRecordResponse"] & Record<string, unknown> =
        JSON.parse(raw);
      const item = Object.fromEntries(
        [
          "deletedAt",
          "cwd",
          "branch",
          "workspaceMode",
          "worktreeReady",
          "workspaceCleanupIntent",
          "cleanedWorktree",
          "rootId",
          "parentId",
        ].map((key) => [key, record[key] ?? null]),
      );
      const attempt = record.startAttempt || {};
      const initial = id + ":initial";
      let initialInputNotSent = false;
      if (
        attempt.submitted === false &&
        attempt.executionOutcome === "unsent" &&
        JSON.stringify(attempt.events) === JSON.stringify([initial])
      ) {
        const counts = database
          .prepare(
            "SELECT count(*) AS total,coalesce(sum(id=? AND status='cancelled'),0) AS cancelled FROM runtime_events WHERE agent=?",
          )
          .get(initial, id);
        initialInputNotSent = counts?.total === 1 && counts.cancelled === 1;
      }
      return {
        id,
        initialInputNotSent,
        workspaceCleanupIntent: item.workspaceCleanupIntent,
        deletedAt: record.deletedAt ?? null,
        cwd: record.cwd ?? null,
        branch: record.branch ?? null,
        workspaceMode: record.workspaceMode ?? null,
        worktreeReady: record.worktreeReady ?? null,
        cleanedWorktree: record.cleanedWorktree ?? null,
        rootId: record.rootId ?? null,
        parentId: record.parentId ?? null,
      };
    });
  });
}

// Evidence only, not a pass gate: reply latency of thread/start and
// thread/resume per account, from the physical stdin flush to the reply line.
export function summarizeProviderThreadReplies(
  stdinEntries: StdinEntry[],
  replies: ReplyEntry[],
) {
  const writes = new Map<string, StdinEntry>();
  for (const entry of stdinEntries) {
    if (!THREAD_REQUEST_METHODS.includes(entry.method)) continue;
    writes.set(
      JSON.stringify([
        entry.providerHandle,
        entry.generation,
        entry.operationId,
        entry.requestId,
      ]),
      entry,
    );
  }
  const latencies = new Map<string, number[]>();
  let unmatchedReplies = 0;
  const answered = new Set();
  for (const reply of replies) {
    const key = JSON.stringify([
      reply.providerHandle,
      reply.generation,
      reply.operationId,
      reply.requestId,
    ]);
    const write = writes.get(key);
    const latencyMs = write
      ? (reply.replyAtNs - write.stdinFlushedAtNs) / 1_000_000
      : NaN;
    if (!write || write.method !== reply.method || !(latencyMs >= 0)) {
      unmatchedReplies += 1;
      continue;
    }
    answered.add(key);
    const group = `${reply.providerHandle} ${reply.method}`;
    latencies.set(group, [...(latencies.get(group) || []), latencyMs]);
  }
  return {
    state: "evidence",
    note: "Per account and method: physical stdin flush to reply arrival, CLOCK_MONOTONIC. Evidence only; not a pass gate.",
    byAccountAndMethod: Object.fromEntries(
      [...latencies].map(([group, values]) => [group, summary(values)]),
    ),
    writesWithoutReply: writes.size - answered.size,
    unmatchedReplies,
  };
}

export function summarizeEventTiming(
  evidence: TimingEvidenceInput,
  allJournalEntries: StdinEntry[],
) {
  const journalEntries = allJournalEntries.filter(
    (entry) => !THREAD_REQUEST_METHODS.includes(entry.method),
  );
  const events = evidence?.events || [];
  const entriesByRequest = new Map<string, StdinEntry[]>();
  const operationCounts = new Map<string, number>();
  for (const entry of journalEntries) {
    const identity = requestIdentityKey(entry.requestId);
    if (identity !== null) {
      const key = JSON.stringify([
        identity,
        entry.generation,
        entry.providerHandle,
      ]);
      const entries = entriesByRequest.get(key) || [];
      entries.push(entry);
      entriesByRequest.set(key, entries);
    }
    operationCounts.set(
      entry.operationId,
      (operationCounts.get(entry.operationId) || 0) + 1,
    );
  }
  const timestampPairs = [];
  const timingGaps = [];
  for (const event of events) {
    const enqueuedAt = event.timing?.enqueuedAt;
    const attemptMatches = [];
    for (const attempt of event.attempts || []) {
      const accountKey = attempt.accountKey;
      const identity = requestIdentityKey(attempt.requestTicket);
      if (identity === null) continue;
      const requestKey = JSON.stringify([
        identity,
        attempt.connectionGeneration,
        `account:${accountKey}`,
      ]);
      for (const entry of entriesByRequest.get(requestKey) || []) {
        if (
          Array.isArray(attempt.events) &&
          attempt.events.includes(event.id) &&
          requestIdentityKey(entry.requestId) === identity &&
          attempt.connectionGeneration === entry.generation &&
          attempt.runAccountKey === accountKey &&
          entry.providerHandle === `account:${accountKey}`
        )
          attemptMatches.push({ entry, attempt });
      }
    }
    if (attemptMatches.length !== 1) {
      timingGaps.push({
        eventId: event.id,
        reason:
          attemptMatches.length === 0
            ? "no exact requestTicket/generation/account/events journal join"
            : "multiple physical-flush journal witnesses matched one event",
        matches: attemptMatches.length,
      });
      continue;
    }
    const match = attemptMatches[0];
    assert.ok(match);
    const { entry, attempt } = match;
    if (operationCounts.get(entry.operationId) !== 1) {
      timingGaps.push({
        eventId: event.id,
        operationId: entry.operationId,
        reason: "duplicate physical-flush operation ID in the scoped journal",
      });
      continue;
    }
    if (entry.method !== "turn/start") {
      timingGaps.push({
        eventId: event.id,
        attemptId: attempt.id,
        method: entry.method,
        reason:
          "no TurnStart physical-flush witness is available for this input",
      });
      continue;
    }
    if (typeof enqueuedAt !== "number" || !Number.isSafeInteger(enqueuedAt)) {
      timingGaps.push({ eventId: event.id, reason: "missing enqueuedAt" });
      continue;
    }
    const latency = (entry.stdinFlushedAtNs - enqueuedAt) / 1_000_000;
    if (!Number.isFinite(latency) || latency < 0) {
      timingGaps.push({
        eventId: event.id,
        reason: "physical flush precedes enqueue or is outside the valid range",
      });
      continue;
    }
    timestampPairs.push({
      eventId: event.id,
      agentId: event.agentId,
      attemptId: attempt.id,
      requestId: entry.requestId,
      operationId: entry.operationId,
      generation: entry.generation,
      providerHandle: entry.providerHandle,
      stdinFlushedAtNs: entry.stdinFlushedAtNs,
      latencyMs: latency,
    });
  }
  const stdinLatencyMs = timestampPairs.map((pair) => pair.latencyMs);
  return {
    readyToProviderStdinMs: {
      state:
        stdinLatencyMs.length === events.length &&
        events.length > 0 &&
        timingGaps.length === 0
          ? "measured"
          : "incomplete",
      samples: summary(stdinLatencyMs),
      requiredP95LimitMs: 2_000,
      note: "Pairs exact-event CLOCK_MONOTONIC `timing.enqueuedAt` with a PID-, boot-, and stage-bounded `studio_provider_stdin_write` journald witness. It joins through the indexed event-to-run input and exact persisted requestTicket, generation, accountKey, and attempt event IDs. Only physical turn/start writes are accepted; missing, duplicate, conflicting, legacy, remote, and turn/steer inputs remain incomplete.",
      exactEvents: events.length,
      timestampPairs: stdinLatencyMs.length,
      uniqueOperationIds: new Set(
        timestampPairs.map((pair) => pair.operationId),
      ).size,
      joins: timestampPairs,
      gaps: timingGaps,
    },
    schedulerQueueWaitMs: {
      state: "unavailable",
      samples: summary([]),
      reason:
        "Rust does not persist per-event scheduler admission and dispatch timestamps.",
    },
    preparationAndDispatchMs: {
      state: "unavailable",
      samples: summary([]),
      reason:
        "Rust does not persist per-event preparation completion and provider write-begin timestamps.",
    },
    nativeSubmitWriteMs: {
      state: "unavailable",
      samples: summary([]),
      reason: "The authenticated Rust Written receipt has no timestamp.",
    },
    providerGenerationMs: {
      state: "unavailable",
      reason:
        "No Rust provider-owned generation-start and generation-complete timestamps are persisted for exact event IDs.",
    },
    providerErrors: {
      events: evidence?.providerErrorEvents || [],
      upstreamLimitMatches: (evidence?.providerErrorEvents || []).filter(
        (item) => showsUpstreamLimit(item.error),
      ),
      aggregateAttribution:
        "Unavailable: no provider-specific aggregate rate-limit diagnostic is exposed.",
    },
  };
}

async function waitForStoppedAgents(
  stream: SyncStream,
  ids: string[],
  deadlineAt: number,
) {
  const expected = new Set(ids);
  const stopped = new Set();
  while (stopped.size < expected.size && performance.now() < deadlineAt) {
    const event = await stream.next(deadlineAt);
    if (event.name !== "resources") continue;
    for (const document of event.stateDocuments || []) {
      const entity = parseStateEntity(document);
      if (!entity || !expected.has(entity.id)) continue;
      if (
        !entity.inFlight &&
        entity.status === "paused" &&
        entity.autoWake === false
      )
        stopped.add(entity.id);
    }
  }
  if (stopped.size !== expected.size)
    throw new Error(
      `Stop event stream verified ${stopped.size}/${expected.size} owned agents before cleanup deadline.`,
    );
}

async function waitForWorkspaceCleanup(
  stream: SyncStream,
  ids: string[],
  deadlineAt: number,
  stateDir: string,
  workspaceId: string,
  leadId: string,
) {
  const expected = new Set(ids);
  const readFinished = async () =>
    finishedWorkspaceCleanupIds(
      await readCurrentTeamChildren(stateDir, workspaceId, leadId, deadlineAt, [
        ...expected,
      ]),
      expected,
    );
  // Cleanup may already be complete before this stream's baseline, so read
  // the current rows first; an event is only needed for a later change.
  let finished = await readFinished();
  while (finished.size < expected.size && performance.now() < deadlineAt) {
    const event = await stream.next(deadlineAt);
    if (event.name !== "resources") continue;
    finished = await readFinished();
  }
  assert.equal(
    finished.size,
    expected.size,
    `Workspace cleanup completion event observed for ${finished.size}/${expected.size} owned agents.`,
  );
}

// A callback shell that never got its release (a failed stage) is released now
// instead of waiting for its full timeout lifetime. A missing FIFO or reader
// means the shell is gone; either way the short exit bound applies.
// The release FIFO of a worker, in its worktree or in the shared-workspace
// scratch directory under the evidence directory.
function ownerBarrierPath(owner: OwnedAgent) {
  return owner.sharedBarrier?.barrierPath ?? owner.worktree?.barrierPath;
}

function releaseUnfinishedCallbacks(owners: OwnedAgent[], journal: Journal) {
  const pending = owners.filter(
    (owner) =>
      ownerBarrierPath(owner) &&
      owner.callbackReadinessObservedAt &&
      !owner.callbackReleasedAt &&
      !journal.stages
        .find((stage) => stage.workers === owner.stageWorkers)
        ?.toolCallbacksVerified?.some(
          (proof) =>
            proof?.agentId === owner.id &&
            proof.callbackProcessesRetired === true,
        ),
  );
  const entries = pending.map((owner) => {
    const path = ownerBarrierPath(owner);
    const stage = journal.stages.find(
      (stage) => stage.workers === owner.stageWorkers,
    );
    assert.ok(
      path && stage?.toolBarrierTokens && owner.stageIndex !== undefined,
    );
    const token = stage.toolBarrierTokens[owner.stageIndex];
    assert.ok(token);
    return { path, token };
  });
  releaseBarriersNow(entries);
  const releasedAt = new Date().toISOString();
  for (const owner of pending) owner.callbackReleasedAt = releasedAt;
}

// The deterministic scratch paths of a shared-workspace worker, so cleanup can
// find them even when the stage failed before its readiness was journaled.
async function sharedBarrierRecord(journalPath: string, owner: OwnedAgent) {
  const evidenceDir = await realpath(dirname(journalPath));
  assert.ok(owner.stageIndex !== undefined);
  const { directory, path } = sharedBarrierPaths(
    evidenceDir,
    owner.stageWorkers,
    owner.stageIndex,
  );
  return {
    barrierDirectory: directory,
    barrierPath: path,
    readinessMarkerPath: join(
      evidenceDir,
      `readiness-markers-${owner.stageWorkers}`,
      readinessMarkerName(owner.stageIndex),
    ),
  };
}

// Removes the run-owned FIFOs and readiness markers of shared-workspace
// workers after their tool processes retired. A FIFO that a process still
// holds open is not removed: that retirement is unproven.
async function removeSharedBarriers(owners: OwnedAgent[]) {
  const paths = owners.map((owner) => {
    assert.ok(owner.sharedBarrier);
    return owner.sharedBarrier.barrierPath;
  });
  const holders = findFifoHolders(paths);
  const stageDirectories = new Set<string>();
  for (const owner of owners) {
    assert.ok(owner.sharedBarrier);
    const { barrierDirectory, barrierPath, readinessMarkerPath } =
      owner.sharedBarrier;
    assert.ok(readinessMarkerPath);
    assert.equal(
      holders.get(barrierPath)?.length,
      0,
      `Owned tool process still holds the release FIFO of ${owner.id}.`,
    );
    for (const [path, remove] of [
      [barrierPath, unlink],
      [barrierDirectory, rmdir],
      [readinessMarkerPath, unlink],
    ] as const) {
      try {
        assert.ok(path);
        await remove(path);
      } catch (error) {
        if (errorCode(error) !== "ENOENT") throw error;
      }
    }
    stageDirectories.add(dirname(barrierDirectory));
    stageDirectories.add(dirname(readinessMarkerPath));
  }
  for (const directory of stageDirectories) {
    try {
      await rmdir(directory);
    } catch (error) {
      if (errorCode(error) !== "ENOENT") throw error;
    }
  }
}

async function verifyOwnedWorktreesRemoved(
  client: Client,
  project: string,
  owners: OwnedAgent[],
  rows: TeamChild[],
  deadlineAt: number,
  journal: Journal,
  journalPath: string,
) {
  const worktrees = owners.filter(
    (owner): owner is OwnedAgent & { worktree: Worktree } =>
      Boolean(owner.worktree?.cwd),
  );
  const sharedWorkers = owners.filter(
    (owner) => !owner.isLead && journal.workerWorkspace === "shared",
  );
  for (const owner of sharedWorkers)
    owner.sharedBarrier ??= await sharedBarrierRecord(journalPath, owner);
  if (!worktrees.length && !sharedWorkers.length) return;
  const byId = new Map(rows.map((row) => [row.id, row]));
  for (const owner of worktrees) {
    const row = byId.get(owner.id);
    assert.ok(
      row?.deletedAt,
      `Owned worker ${owner.id} has no durable deletedAt after cleanup.`,
    );
    assert.equal(
      row.workspaceCleanupIntent,
      null,
      `Owned worker ${owner.id} still has a workspace cleanup intent.`,
    );
    assert.equal(
      normalizedWorktreePath(row.cwd),
      owner.worktree.cwd,
      `Owned worker ${owner.id} worktree identity changed before cleanup.`,
    );
    assert.equal(
      row.branch,
      owner.worktree.branch,
      `Owned worker ${owner.id} branch identity changed before cleanup.`,
    );
    assert.equal(
      row.workspaceMode,
      SCALE_WORKSPACE_MODE,
      `Owned worker ${owner.id} did not persist worktree mode.`,
    );
  }
  const processRetirementDeadlines = [];
  const callbackOwners = [...worktrees, ...sharedWorkers];
  releaseUnfinishedCallbacks(callbackOwners, journal);
  for (const owner of callbackOwners) {
    if (
      byId.get(owner.id)?.initialInputNotSent === true &&
      owner.exchanges.length === 0
    )
      continue;
    const stage = journal.stages.find(
      (candidate) => candidate.workers === owner.stageWorkers,
    );
    const expectedOutput = `STUDIO_SCALE_${journal.runId.slice(0, 8)}_${owner.stageWorkers}_${owner.stageIndex}_TOOL_OK`;
    // The workload verifies the transcript before deletion; deleted chats no
    // longer expose that API. Reuse its exact, persisted callback evidence.
    const completed = stage?.toolCallbacksVerified?.some(
      (proof) =>
        proof?.agentId === owner.id &&
        proof.runId === journal.runId &&
        proof.successfulOutput === expectedOutput &&
        proof.callbackProcessesRetired === true,
    );
    if (completed) continue;
    // With no completion evidence, only an observed FIFO marker provides the
    // timeout's latest start. Missing or unknown evidence remains fail closed.
    processRetirementDeadlines.push(
      callbackProcessRetirementDelayMs({
        releasedAt: owner.callbackReleasedAt,
        readinessObservedAt:
          owner.callbackReadinessObservedAt ??
          owner.worktree?.readinessObservedAt ??
          owner.sharedBarrier?.readinessObservedAt,
      }),
    );
  }
  const retirementWaitMs = Math.max(0, ...processRetirementDeadlines);
  if (retirementWaitMs > 0) {
    assert.ok(
      performance.now() + retirementWaitMs < deadlineAt,
      "Cannot prove callback retirement within cleanup budget.",
    );
    await new Promise((resolvePromise) =>
      setTimeout(resolvePromise, retirementWaitMs),
    );
  }

  await removeSharedBarriers(sharedWorkers);
  if (!worktrees.length) return;

  const timeout = () => {
    const remaining = Math.floor(deadlineAt - performance.now());
    assert.ok(
      remaining > 0,
      "Cleanup deadline elapsed during worktree retirement.",
    );
    return Math.min(REQUEST_DEADLINE_MS, remaining);
  };
  const runGit = (cwd: string, args: string[]) => {
    const result = spawnSync("git", ["-C", cwd, ...args], {
      encoding: "utf8",
      timeout: timeout(),
      maxBuffer: 2 * 1024 * 1024,
    });
    if (result.error || result.status !== 0)
      throw Object.assign(
        new Error(
          `Owned worktree Git command failed: ${String((result.error instanceof Error ? result.error.message : undefined) || result.stderr || result.status).slice(0, 500)}`,
        ),
        {
          unknown:
            errorCode(result.error) === "ETIMEDOUT" || result.signal != null,
        },
      );
    return result.stdout;
  };
  const parseRegistrations = (text: string) =>
    text
      .split(/\n\n+/)
      .filter(Boolean)
      .map((block) => {
        const path = block.match(/^worktree (.+)$/m)?.[1];
        const branch = block.match(/^branch (.+)$/m)?.[1] || null;
        return path ? { path: resolve(path), branch } : null;
      })
      .filter((row) => row !== null);
  let registered = parseRegistrations(
    runGit(project, ["worktree", "list", "--porcelain"]),
  );
  const projectHead = journal.projectHead;
  assert.match(
    projectHead || "",
    /^[0-9a-f]{40,64}$/i,
    "Ownership journal has no frozen project HEAD.",
  );
  const runIntent = async (
    owner: OwnedAgent & { worktree: Worktree },
    key: "removeIntent" | "branchDeleteIntent",
    args: string[],
    inspectDone: () => Promise<boolean>,
  ) => {
    const existing = owner.worktree[key];
    const intent: Intent =
      existing ||
      (owner.worktree[key] = {
        state: "prepared",
        args,
        preparedAt: new Date().toISOString(),
      });
    if (
      existing &&
      journal.cleanup.state === "resuming" &&
      intent.state === "prepared"
    )
      intent.state = "unknown";
    if (intent.state === "confirmed") {
      assert.ok(
        await inspectDone(),
        `Previously confirmed owned worktree ${key} no longer verifies for ${owner.id}.`,
      );
      return;
    }
    if (intent.state === "unknown") {
      if (await inspectDone()) {
        intent.state = "confirmed";
        intent.resolvedAt = new Date().toISOString();
        await saveJournal(journalPath, journal);
        return;
      }
      throw new Error(
        `Owned worktree ${key} outcome remains unknown for ${owner.id}; inspection did not prove completion and no command was retried.`,
      );
    }
    assert.equal(
      intent.state,
      "prepared",
      `Refusing ${key} state ${intent.state}.`,
    );
    await saveJournal(journalPath, journal);
    try {
      assert.ok(intent.args);
      runGit(project, intent.args);
      if (intent.args[0] === "worktree" && intent.args[1] === "remove") {
        registered = registered.filter(
          (item) => item.path !== resolve(args.at(-1) ?? ""),
        );
      }
      intent.state = "confirmed";
      intent.finishedAt = new Date().toISOString();
      await saveJournal(journalPath, journal);
      assert.ok(
        await inspectDone(),
        `Owned worktree ${key} did not verify for ${owner.id}.`,
      );
    } catch (error) {
      intent.state =
        typeof error === "object" &&
        error !== null &&
        "unknown" in error &&
        error.unknown
          ? "unknown"
          : "failed";
      intent.error = String(
        (error instanceof Error ? error.message : undefined) || error,
      ).slice(0, 500);
      await saveJournal(journalPath, journal);
      throw error;
    }
  };
  for (const owner of worktrees) {
    assert.ok(
      typeof owner.worktree.branch === "string" && owner.worktree.branch,
      `Owned worktree ${owner.id} has no durable branch identity.`,
    );
    const cwd = resolve(owner.worktree.cwd);
    const candidates = registered.filter((item) => {
      const rel = relative(item.path, cwd);
      return rel === "" || (!rel.startsWith(`..${sep}`) && !isAbsolute(rel));
    });
    const owned = candidates.filter((item) => {
      const parts = item.path.split(sep).filter(Boolean);
      return parts.at(-1) === owner.id && parts.at(-2) === "codex-agents";
    });
    if (!owner.worktree.registeredRoot) {
      assert.equal(
        owned.length,
        1,
        `Could not identify exactly one verifier-owned registered worktree for ${owner.id}.`,
      );
      const registration = owned[0];
      assert.ok(registration);
      owner.worktree.registeredRoot = registration.path;
      await saveJournal(journalPath, journal);
    }
    const root = resolve(owner.worktree.registeredRoot);
    if (owned.length) {
      assert.ok(
        owned.length === 1 && owned[0]?.path === root,
        `Owned worktree registration changed for ${owner.id}.`,
      );
      assert.equal(
        owned[0]?.branch,
        `refs/heads/${owner.worktree.branch}`,
        `Owned worktree branch identity changed for ${owner.id}.`,
      );
    } else {
      assert.ok(
        owner.worktree.removeIntent,
        `Owned worktree registration disappeared before a journaled removal for ${owner.id}.`,
      );
    }
    assert.equal(rootParts(root, 0), owner.id);
    assert.equal(rootParts(root, 1), "codex-agents");
    const rootExists = async () => {
      try {
        await lstat(root);
        return true;
      } catch (error) {
        if (errorCode(error) === "ENOENT") return false;
        throw error;
      }
    };

    const barrierDirectory = owner.worktree.barrierDirectory;
    const barrierPath = owner.worktree.barrierPath;
    if (await rootExists()) {
      const rootInfo = await lstat(root);
      assert.ok(
        rootInfo.isDirectory(),
        `Owned worktree root is not a real directory: ${root}`,
      );
      if (barrierDirectory || barrierPath) {
        assert.ok(
          barrierDirectory && barrierPath,
          "Owned callback artifact paths are incomplete.",
        );
        assert.equal(
          resolve(barrierDirectory),
          join(cwd, `.codex-scale-${journal.runId}-${owner.stageIndex}`),
          "Owned barrier directory does not match its journaled worker index.",
        );
        assert.equal(
          resolve(barrierPath),
          join(resolve(barrierDirectory), "release.fifo"),
        );
        const before = runGit(root, [
          "status",
          "--porcelain",
          "--untracked-files=all",
          "--ignored=matching",
        ]);
        const allowed = new Set([
          `${relative(root, barrierDirectory)}/`,
          relative(root, barrierPath),
        ]);
        for (const line of before.trim().split("\n").filter(Boolean)) {
          const entry = line.slice(3).replaceAll('"', "");
          assert.ok(
            allowed.has(entry),
            `Owned worktree contains an unexpected change: ${entry}`,
          );
        }
        try {
          const directoryInfo = await lstat(barrierDirectory);
          assert.ok(
            directoryInfo.isDirectory(),
            "Owned FIFO scratch path is not a directory.",
          );
          const entries = await readdir(barrierDirectory);
          assert.ok(
            entries.length === 0 ||
              (entries.length === 1 && entries[0] === "release.fifo"),
            "Owned FIFO directory contains unexpected files.",
          );
          if (entries.length) {
            const fifoInfo = await lstat(barrierPath);
            assert.ok(fifoInfo.isFIFO(), "Owned barrier path is not a FIFO.");
            await unlink(barrierPath);
          }
          await rmdir(barrierDirectory);
        } catch (error) {
          if (errorCode(error) !== "ENOENT") throw error;
        }
      }
      assert.equal(
        runGit(root, [
          "status",
          "--porcelain",
          "--untracked-files=all",
          "--ignored=matching",
        ]).trim(),
        "",
        `Owned worktree has changes after removing the exact callback artifact for ${owner.id}.`,
      );
      assert.equal(
        runGit(root, ["rev-parse", "HEAD"]).trim(),
        projectHead,
        `Owned worktree HEAD changed from the frozen source base for ${owner.id}.`,
      );
    } else {
      assert.ok(
        owner.worktree.removeIntent,
        `Owned worktree disappeared before a journaled removal for ${owner.id}.`,
      );
    }
    await runIntent(
      owner,
      "removeIntent",
      ["worktree", "remove", "--", root],
      async () =>
        !(await rootExists()) && !registered.some((item) => item.path === root),
    );
    registered = registered.filter((item) => item.path !== root);
    const branchRef = `refs/heads/${owner.worktree.branch}`;
    const branchExists = () => {
      const result = spawnSync(
        "git",
        ["-C", project, "show-ref", "--verify", "--quiet", branchRef],
        {
          encoding: "utf8",
          timeout: timeout(),
        },
      );
      if (
        result.error ||
        !([0, 1] as const).some((code) => code === result.status)
      )
        throw new Error(
          `Could not inspect owned branch ${branchRef}: ${String((result.error instanceof Error ? result.error.message : undefined) || result.stderr || result.status)}`,
        );
      return result.status === 0;
    };
    if (!owner.worktree.branchDeleteIntent && !branchExists()) {
      owner.worktree.branchDeleteIntent = {
        state: "confirmed",
        inspectedAbsent: true,
        finishedAt: new Date().toISOString(),
      };
      await saveJournal(journalPath, journal);
    } else {
      await runIntent(
        owner,
        "branchDeleteIntent",
        ["branch", "-d", "--", owner.worktree.branch],
        async () => !branchExists(),
      );
    }
    owner.worktree.cleanup = "verified-removed";
    await saveJournal(journalPath, journal);
  }
  const finalRegistered = parseRegistrations(
    runGit(project, ["worktree", "list", "--porcelain"]),
  );
  const survivors = worktrees.filter((owner) =>
    finalRegistered.some(
      (item) => item.path === resolve(owner.worktree.registeredRoot ?? ""),
    ),
  );
  assert.equal(
    survivors.length,
    0,
    `Git still registers ${survivors.length} owned worktree(s) after retirement.`,
  );
  for (const owner of worktrees) {
    try {
      assert.ok(owner.worktree.registeredRoot);
      await lstat(owner.worktree.registeredRoot);
      throw new Error(
        `Owned worktree path still exists after retirement: ${owner.worktree.registeredRoot}`,
      );
    } catch (error) {
      if (errorCode(error) !== "ENOENT") throw error;
    }
  }
}

function rootParts(path: string, indexFromEnd: number) {
  return resolve(path)
    .split(sep)
    .filter(Boolean)
    .at(-1 - indexFromEnd);
}

async function verifyBackendIdentity(client: Client, journal: Journal) {
  const desktop = await client.get<Schema["DesktopResponse"]>("/api/desktop");
  const sync =
    await client.get<Schema["SyncIdentityResponse"]>("/api/sync/identity");
  assert.equal(
    "backendKind" in desktop ? desktop.backendKind : undefined,
    "rust",
    "Cleanup target is not the Rust backend.",
  );
  assert.equal(
    resolve(desktop.stateDir),
    journal.backendIdentity?.stateDir,
    "Cleanup refused because the backend state directory changed.",
  );
  assert.equal(
    sync.workspaceId,
    journal.ownerBaseline?.workspaceId,
    "Cleanup refused because the backend workspace identity changed.",
  );
  return {
    at: new Date().toISOString(),
    pid: desktop.pid,
    stateDir: resolve(desktop.stateDir),
    workspaceId: sync.workspaceId,
    backendBuild: desktop.backendBuild || null,
  };
}

// `onlyStage` limits the audit and deletion to one finished stage.
async function cleanupOwned(
  client: Client,
  journal: Journal,
  journalPath: string,
  budgetMs: number,
  stream: SyncStream | null | undefined,
  onlyStage: Stage | null = null,
) {
  assert.ok(journal.backendIdentity && journal.ownerBaseline);

  const deadlineAt = performance.now() + budgetMs;
  client.setDeadline(deadlineAt);
  const owners = journal.ownedAgents || [];
  if (owners.length)
    journal.cleanup.backendIdentity = await verifyBackendIdentity(
      client,
      journal,
    );
  const byId = new Map(owners.map((owner) => [owner.id, owner]));
  const stages = onlyStage ? [onlyStage] : journal.stages || [];
  for (const stage of stages) {
    assert.ok(stage.leadId);
    if (performance.now() >= deadlineAt)
      throw new Error(
        "Cleanup deadline elapsed before every stage was audited.",
      );
    const stageOwners = [stage.leadId, ...(stage.agentIds || [])]
      .map((id) => byId.get(id))
      .filter((owner) => owner !== undefined);
    if (!stageOwners.length) continue;
    const cleanup = (stage.cleanup ||= { state: "pending" });
    const lead = byId.get(stage.leadId);
    const initialRows = await readCurrentTeamChildren(
      journal.backendIdentity.stateDir,
      journal.ownerBaseline.workspaceId,
      stage.leadId,
      deadlineAt,
    );
    const initialById = new Map(initialRows.map((row) => [row.id, row]));
    const persistedOwners = [];
    for (const owner of stageOwners) {
      const row = initialById.get(owner.id);
      if (!row) {
        if (["prepared", "failed"].includes(owner.create?.state))
          owner.cleanup.state = "verified-not-created";
        else {
          owner.cleanup.state = "unresolved-create";
          owner.cleanup.error =
            "No authoritative runtime_agents row resolves the create intent; siblings are cleaned without retrying this identity.";
        }
        continue;
      }
      assert.equal(
        row.rootId,
        stage.leadId,
        `Owned agent ${owner.id} changed team identity.`,
      );
      persistedOwners.push(owner);
      if (
        row.workspaceMode === SCALE_WORKSPACE_MODE &&
        row.worktreeReady === true &&
        typeof row.cwd === "string" &&
        isAbsolute(row.cwd) &&
        resolve(row.cwd) !== journal.project
      ) {
        if (owner.worktree) {
          assert.equal(
            resolve(owner.worktree.cwd),
            resolve(row.cwd),
            `Owned worktree identity changed for ${owner.id}.`,
          );
          // Branch is private stored metadata, absent from SyncAgentEntity.
          // Bind it from this existing workspace-fenced cleanup snapshot.
          if (owner.worktree.branch !== undefined)
            assert.equal(owner.worktree.branch, row.branch);
          owner.worktree.branch = row.branch;
        } else
          owner.worktree = {
            cwd: resolve(row.cwd),
            branch: row.branch,
            cleanup: "pending",
          };
        if (!owner.isLead) {
          assert.ok(owner.worktree);
          owner.worktree.barrierDirectory = join(
            resolve(row.cwd),
            `.codex-scale-${journal.runId}-${owner.stageIndex}`,
          );
          owner.worktree.barrierPath = join(
            owner.worktree.barrierDirectory,
            "release.fifo",
          );
        }
      }
    }
    await saveJournal(journalPath, journal);
    const foreignChildren = foreignLiveTeamRows(
      initialRows,
      new Set(stageOwners.map((owner) => owner.id)),
    );
    stage.cleanup ||= {};
    stage.cleanup.foreignChildrenObserved = foreignChildren.map(
      (row) => row.id,
    );
    const snapshot = new Map<string, Schema["AgentEntityDto"] | null>();
    await forEachBounded(persistedOwners, 32, async (owner) => {
      const row = initialById.get(owner.id);
      snapshot.set(
        owner.id,
        row?.deletedAt ? null : await client.agentSnapshot(owner.id),
      );
    });
    const present = persistedOwners.filter((owner) => snapshot.get(owner.id));
    if (!present.length) {
      const pendingCleanupIds = persistedOwners
        .filter((owner) => initialById.get(owner.id)?.workspaceCleanupIntent)
        .map((owner) => owner.id);
      if (pendingCleanupIds.length) {
        if (!stream)
          throw new Error(
            "Resumed workspace cleanup needs the existing state stream.",
          );
        await waitForWorkspaceCleanup(
          stream,
          pendingCleanupIds,
          deadlineAt,
          journal.backendIdentity.stateDir,
          journal.ownerBaseline.workspaceId,
          stage.leadId,
        );
      }
      await assertNoForeignTeamChildren(
        journal,
        stage,
        persistedOwners,
        deadlineAt,
        "beforeMutation",
      );
      const rows = await readCurrentTeamChildren(
        journal.backendIdentity.stateDir,
        journal.ownerBaseline.workspaceId,
        stage.leadId,
        deadlineAt,
      );
      await verifyOwnedWorktreesRemoved(
        client,
        journal.project,
        persistedOwners,
        rows,
        deadlineAt,
        journal,
        journalPath,
      );
      await assertNoForeignTeamChildren(
        journal,
        stage,
        persistedOwners,
        deadlineAt,
        "afterCleanup",
      );
      for (const owner of persistedOwners)
        owner.cleanup.state = "verified-deleted";
      cleanup.state = "verified-absent";
      await saveJournal(journalPath, journal);
      continue;
    }

    // Every present owner, lead included, gets one stop command with
    // descendants off. No subtree stop is issued, so a foreign child that
    // appears after the audit cannot be stopped by this cleanup. Latency is
    // measured only for the completed workers.
    const workerOwners = present.filter((owner) => !owner.isLead);
    const measureWorkerStops =
      stage.state === "completed" &&
      workerOwners.length === (stage.agentIds || []).length;
    const workerStopLatencies: { agentId: string; elapsedMs: number }[] = [];
    for (let offset = 0; offset < present.length; offset += 32) {
      const batch = present.slice(offset, offset + 32);
      const send = [];
      for (const owner of batch) {
        const existing = owner.cleanup.stop;
        const intent = (owner.cleanup.stop ||= {
          state: "prepared",
          targetId: owner.id,
          descendants: false,
          preparedAt: new Date().toISOString(),
        });
        Object.assign(
          intent,
          resumeIntentState(
            intent,
            Boolean(existing) && journal.cleanup.state === "resuming",
          ),
        );
        if (
          measureWorkerStops &&
          !owner.isLead &&
          intent.state === "confirmed" &&
          Number.isFinite(intent.elapsedMs)
        )
          workerStopLatencies.push({
            agentId: owner.id,
            elapsedMs: intent.elapsedMs ?? 0,
          });
        else if (intent.state === "prepared") send.push(owner);
      }
      await saveJournal(journalPath, journal);
      const results = await Promise.allSettled(
        send.map(async (owner) => {
          const intent = owner.cleanup.stop;
          assert.ok(intent);
          try {
            const stopped = await client.request<Schema["StopResponse"]>(
              "POST",
              "/api/stop",
              {
                id: owner.id,
                descendants: false,
              },
            );
            intent.state = "confirmed";
            intent.status =
              "status" in stopped.value &&
              typeof stopped.value.status === "string"
                ? stopped.value.status
                : null;
            intent.elapsedMs = stopped.elapsedMs;
            if (measureWorkerStops && !owner.isLead)
              workerStopLatencies.push({
                agentId: owner.id,
                elapsedMs: stopped.elapsedMs,
              });
          } catch (error) {
            intent.state =
              typeof error === "object" &&
              error !== null &&
              "unknown" in error &&
              error.unknown
                ? "unknown"
                : "failed";
            intent.error = String(
              (error instanceof Error ? error.message : undefined) || error,
            ).slice(0, 500);
          }
        }),
      );
      await saveJournal(journalPath, journal);
      if (results.some((result) => result.status === "rejected"))
        throw new Error("A worker stop observer failed unexpectedly.");
    }

    const verifiedStopped = new Map(
      stageOwners.map((owner) => [owner.id, snapshot.get(owner.id) || null]),
    );
    await forEachBounded(stageOwners, 32, async (owner) => {
      verifiedStopped.set(owner.id, await client.agentSnapshot(owner.id));
    });
    let active = stageOwners.filter((owner) => {
      const row = verifiedStopped.get(owner.id);
      return (
        row &&
        (row.inFlight || row.status !== "paused" || row.autoWake !== false)
      );
    });
    if (active.length && stream) {
      await waitForStoppedAgents(
        stream,
        active.map((owner) => owner.id),
        Math.min(deadlineAt, performance.now() + 10_000),
      ).catch((error) => {
        cleanup.workerStopObservationError = String(
          (error instanceof Error ? error.message : undefined) || error,
        ).slice(0, 500);
      });
      await forEachBounded(stageOwners, 32, async (owner) => {
        verifiedStopped.set(owner.id, await client.agentSnapshot(owner.id));
      });
      active = stageOwners.filter((owner) => {
        const row = verifiedStopped.get(owner.id);
        return (
          row &&
          (row.inFlight || row.status !== "paused" || row.autoWake !== false)
        );
      });
    }

    if (active.length)
      throw new Error(
        `Stop outcome remains unresolved for ${active.length} owned agent(s); deletion withheld and no stop is retried.`,
      );
    // Fresh authoritative audit immediately before the delete command. The
    // client check is not atomic with the server; it refuses only a foreign or
    // late child that is already visible.
    await assertNoForeignTeamChildren(
      journal,
      stage,
      persistedOwners,
      deadlineAt,
      "beforeMutation",
    );
    stage.workerStopApiLatencyMs = {
      agents: workerStopLatencies,
      ...summary(workerStopLatencies.map((sample) => sample.elapsedMs)),
    };
    await saveJournal(journalPath, journal);

    const deleteTarget =
      lead && verifiedStopped.get(lead.id) ? lead : present[0];
    assert.ok(deleteTarget);
    const existingDeleteIntent = cleanup.delete;
    const deleteIntent = (cleanup.delete ||= {
      state: "prepared",
      targetId: deleteTarget.id,
      preparedAt: new Date().toISOString(),
    });
    Object.assign(
      deleteIntent,
      resumeIntentState(
        deleteIntent,
        Boolean(existingDeleteIntent) && journal.cleanup.state === "resuming",
      ),
    );
    if (deleteIntent.state === "unknown" && deleteIntent.error) {
      await saveJournal(journalPath, journal);
    }
    if (deleteIntent.state === "prepared") {
      await saveJournal(journalPath, journal);
      try {
        const deleted = await client.request(
          "POST",
          "/api/conversation/delete",
          { id: deleteIntent.targetId },
        );
        deleteIntent.state = "confirmed";
        deleteIntent.elapsedMs = deleted.elapsedMs;
      } catch (error) {
        deleteIntent.state =
          typeof error === "object" &&
          error !== null &&
          "unknown" in error &&
          error.unknown
            ? "unknown"
            : "failed";
        deleteIntent.error = String(
          (error instanceof Error ? error.message : undefined) || error,
        ).slice(0, 500);
        await saveJournal(journalPath, journal);
        throw error;
      }
      await saveJournal(journalPath, journal);
    }
    if (!new Set(["confirmed", "unknown"]).has(deleteIntent.state))
      throw new Error(
        `Cleanup delete is ${deleteIntent.state}; refusing another delete command.`,
      );
    const cleanupWorkerIds = persistedOwners
      .filter((owner) => initialById.get(owner.id)?.workspaceCleanupIntent)
      .map((owner) => owner.id);
    if (cleanupWorkerIds.length) {
      if (!stream)
        throw new Error(
          "Managed workspace cleanup needs the existing state stream to resolve its durable intent.",
        );
      await waitForWorkspaceCleanup(
        stream,
        cleanupWorkerIds,
        deadlineAt,
        journal.backendIdentity.stateDir,
        journal.ownerBaseline.workspaceId,
        stage.leadId,
      );
    }
    const cleanupRows = await readCurrentTeamChildren(
      journal.backendIdentity.stateDir,
      journal.ownerBaseline.workspaceId,
      stage.leadId,
      deadlineAt,
    );
    const cleanupById = new Map(cleanupRows.map((row) => [row.id, row]));
    const survivors = persistedOwners.filter(
      (owner) => !cleanupById.get(owner.id)?.deletedAt,
    );
    if (survivors.length)
      throw new Error(
        `Delete outcome remains unresolved for ${survivors.length} owned agent(s); no delete is retried.`,
      );
    await verifyOwnedWorktreesRemoved(
      client,
      journal.project,
      persistedOwners,
      cleanupRows,
      deadlineAt,
      journal,
      journalPath,
    );
    await assertNoForeignTeamChildren(
      journal,
      stage,
      persistedOwners,
      deadlineAt,
      "afterCleanup",
    );
    cleanup.state = "verified-deleted";
    cleanup.finishedAt = new Date().toISOString();
    for (const owner of persistedOwners) {
      const workerStop = owner.cleanup?.stop;
      owner.cleanup = {
        state: "verified-deleted",
        at: cleanup.finishedAt,
        stopCommand: workerStop?.state || null,
        stopElapsedMs: workerStop?.elapsedMs ?? null,
        deleteCommand: deleteIntent.state,
      };
    }
    await saveJournal(journalPath, journal);
  }
  const terminalOwnerCleanupStates = new Set([
    "verified-deleted",
    "verified-not-created",
    "verified-absent",
  ]);
  const unresolved = owners.filter(
    (owner) => !terminalOwnerCleanupStates.has(owner.cleanup?.state),
  );
  if (unresolved.length)
    throw new Error(
      `Cleanup did not verify ${unresolved.length} owned agent(s).`,
    );
  if (owners.length)
    journal.cleanup.retainedInputs = verifyRetainedInputsAfterCleanup(
      journal,
      deadlineAt,
    );
}

// Runs once every owned delete is verified. One exact-identity read proves that
// no owned input is still retained and that foreign retained inputs still equal
// the pre-write baseline. Paused-lead notices must have been cancelled by the
// owned deletes; a surviving one fails the run.
function verifyRetainedInputsAfterCleanup(
  journal: Journal,
  deadlineAt: number,
) {
  assert.ok(journal.backendIdentity && journal.ownerBaseline);

  assert.ok(
    journal.foreignRetainedInputBaseline,
    "No pre-write foreign retained-input baseline exists; post-cleanup inputs are unproven.",
  );
  const classification = classifyRetainedInputs({
    rows: readRetainedInputRows(
      journal.backendIdentity.stateDir,
      journal.ownerBaseline.workspaceId,
      deadlineAt,
    ),
    baseline: journal.foreignRetainedInputBaseline.identities,
    ownedAgentIds: new Set(journal.ownedAgents.map((owner) => owner.id)),
    parentAgentIds: new Set(
      journal.stages.flatMap((stage) => (stage.leadId ? [stage.leadId] : [])),
    ),
    phase: "afterCleanup",
  });
  return { at: new Date().toISOString(), ...classification };
}

// Fresh authoritative audit of the lead's live team. `owners` is the journal-
// owned set that may remain live; any other live row fails the stage.
async function assertNoForeignTeamChildren(
  journal: Journal,
  stage: Stage,
  owners: OwnedAgent[],
  deadlineAt: number,
  auditKey: "beforeMutation" | "afterCleanup",
) {
  assert.ok(journal.backendIdentity && journal.ownerBaseline);

  assert.ok(stage.leadId);
  const rows = await readCurrentTeamChildren(
    journal.backendIdentity.stateDir,
    journal.ownerBaseline.workspaceId,
    stage.leadId,
    deadlineAt,
  );
  const owned = new Set(owners.map((owner) => owner.id));
  const foreign = foreignLiveTeamRows(rows, owned);
  assert.equal(
    foreign.length,
    0,
    `Refusing managed-tree cleanup (${auditKey}): authoritative SQLite contains ${foreign.length} foreign or unjournaled child row(s): ${foreign.map((row) => row.id).join(", ")}.`,
  );
  stage.cleanup ||= {};
  stage.cleanup[auditKey] = {
    at: new Date().toISOString(),
    rowsObserved: rows.filter((row) => row.deletedAt == null).length,
    ownedCurrentRows: rows.filter(
      (row) => row.deletedAt == null && owned.has(row.id),
    ).length,
    foreignRows: 0,
    source:
      "single read-only SQLite runtime_agents snapshot, workspace identity checked",
  };
}

async function resumeCleanup(
  origin: string,
  journalPath: string,
  journal: Journal,
) {
  assert.ok(journal.backendIdentity && journal.ownerBaseline);

  assert.ok(
    journal.ownedAgents?.length,
    "Journal contains no verifier-owned agent identities.",
  );
  const client = await createClient(origin, journal, journalPath);
  client.cleanupMode = true;
  const cleanupDeadline = performance.now() + CLEANUP_DEADLINE_MS;
  client.setDeadline(cleanupDeadline);
  journal.cleanup.backendIdentity = await verifyBackendIdentity(
    client,
    journal,
  );
  const diskBeforeCleanup = await readFilesystemSnapshot(journal.project);
  assert.ok(
    diskBeforeCleanup.availableBytes >= FREE_DISK_FLOOR_BYTES,
    `Free disk space is below the ${FREE_DISK_FLOOR_BYTES}-byte floor before resumed cleanup.`,
  );
  const stream = await startSyncResourceStream(
    client,
    journal,
    journalPath,
    false,
  );
  journal.cleanup.state = "resuming";
  await saveJournal(journalPath, journal);
  try {
    await cleanupOwned(
      client,
      journal,
      journalPath,
      Math.max(0, cleanupDeadline - performance.now()),
      stream,
    );
    const diskAfterCleanup = await readFilesystemSnapshot(journal.project);
    assert.ok(
      diskAfterCleanup.availableBytes >= FREE_DISK_FLOOR_BYTES,
      `Free disk space is below the ${FREE_DISK_FLOOR_BYTES}-byte floor after resumed cleanup.`,
    );
    journal.cleanup.filesystemDisk = {
      at: diskAfterCleanup.at,
      availableBytesBefore: diskBeforeCleanup.availableBytes,
      availableBytesAfter: diskAfterCleanup.availableBytes,
      usedDeltaBytes: diskAfterCleanup.usedBytes - diskBeforeCleanup.usedBytes,
      deltaAttribution:
        "shared-filesystem delta; not attributed solely to owned worktrees",
    };
  } finally {
    await stream.close();
  }
  journal.cleanup.state = "verified";
  journal.cleanup.resumedAt = new Date().toISOString();
  await saveJournal(journalPath, journal);
  process.stdout.write(
    `${JSON.stringify({ cleanup: "verified", evidence: journalPath, ownedAgents: journal.ownedAgents.length })}\n`,
  );
}

function aggregateMetrics(journal: Journal) {
  const stages = journal.stages || [];
  return {
    apiP95Ms: Object.fromEntries(
      ["create", "message"].map((key) => [
        key,
        percentile(
          stages.flatMap((stage) =>
            key === "create"
              ? stage.createLatenciesMs || []
              : stage.secondExchangeLatenciesMs || [],
          ),
          0.95,
        ),
      ]),
    ),
    apiP95LimitMs: API_P95_LIMIT_MS,
    readyToProviderStdinMs: {
      byStage: stages.map((stage) => ({
        workers: stage.workers,
        ...(stage.timingEvidence?.readyToProviderStdinMs || {
          state: "unavailable",
          reason: journal.metricsGaps[0],
        }),
      })),
    },
    providerThreadReplyMs: stages.map((stage) => ({
      workers: stage.workers,
      ...(stage.timingEvidence?.providerThreadReplyMs || {
        state: "unavailable",
      }),
    })),
    observedCompleteTurnDeliveryMs: Object.fromEntries(
      stages.map((stage) => [
        stage.workers,
        stage.completeTurnDeliveryLatencyMs || null,
      ]),
    ),
    providerGenerationMs: stages.map((stage) => ({
      workers: stage.workers,
      ...(stage.timingEvidence?.providerGenerationMs || {
        state: "unavailable",
      }),
    })),
    schedulerQueueWaitMs: stages.map((stage) => ({
      workers: stage.workers,
      ...(stage.timingEvidence?.schedulerQueueWaitMs || {
        state: "unavailable",
      }),
    })),
    preparationAndDispatchMs: stages.map((stage) => ({
      workers: stage.workers,
      ...(stage.timingEvidence?.preparationAndDispatchMs || {
        state: "unavailable",
      }),
    })),
    operationLockWaitHold: stages.map((stage) => ({
      workers: stage.workers,
      before: stage.diagnosticsBefore?.runtimeLockOperations || null,
      after: stage.diagnosticsAfter?.runtimeLockOperations || null,
    })),
    queueAndBlockingPool: stages.map((stage) => ({
      workers: stage.workers,
      before: {
        queues: stage.diagnosticsBefore?.queues || null,
        blockingPool: stage.diagnosticsBefore?.blockingPoolMetrics || null,
      },
      after: {
        queues: stage.diagnosticsAfter?.queues || null,
        blockingPool: stage.diagnosticsAfter?.blockingPoolMetrics || null,
      },
    })),
    sqliteTransactionAndCommit: stages.map((stage) => ({
      workers: stage.workers,
      before: stage.diagnosticsBefore?.sqliteContention || null,
      after: stage.diagnosticsAfter?.sqliteContention || null,
      lockErrorDelta: stage.sqliteLockErrorDelta ?? null,
      lockErrorEvidence: stage.sqliteLockErrorsEvidence || null,
    })),
    workerStopApiLatency: stages.map((stage) => ({
      workers: stage.workers,
      ...(stage.workerStopApiLatencyMs || {
        agents: [],
        samples: 0,
        p95Ms: null,
      }),
      note: "One POST /api/stop result per completed worker; this measures post-workload cleanup, not active interrupt latency.",
    })),
    providerErrorsAndUpstreamLimits: stages.map((stage) => ({
      workers: stage.workers,
      providerErrors: stage.eventEvidence?.providerErrorEvents || [],
      upstreamLimitMatches:
        stage.timingEvidence?.providerErrors?.upstreamLimitMatches || [],
      attributionGap: "no aggregate provider-specific limit diagnostic found",
    })),
    concurrentProviderTurns: stages.map((stage) => ({
      workers: stage.workers,
      observedPeakOpenTurns: stage.maxConcurrentTurns || 0,
      provedAllWorkersConcurrent:
        stage.concurrentWorkers?.workers === stage.workers,
      criterion: stage.concurrentWorkers || null,
    })),
  };
}

if (import.meta.url === pathToFileURL(process.argv[1] || "").href) {
  const args = process.argv.slice(2);
  if (args.includes("--help") || args.includes("-h"))
    process.stdout.write(`${usage()}\n`);
  else
    runScaleVerifier(main, {
      onInterrupt(signal) {
        interruptRequested = true;
        workloadAbort.abort(
          new Error(`Received ${signal}; stopping owned work.`),
        );
        killBarrierProcesses();
      },
    }).catch((error) => {
      process.stderr.write(
        `Live Rust chat scale verifier failed: ${error?.stack || error}\n`,
      );
      process.exitCode = 1;
    });
}

function remainingOwnership(journalPath: string, journal: Journal) {
  return {
    evidence: journalPath,
    cleanup: journal.cleanup,
    agents: (journal.ownedAgents || [])
      .filter(
        (owner) =>
          !["verified-deleted", "verified-not-created"].includes(
            owner.cleanup?.state,
          ),
      )
      .map((owner) => ({ id: owner.id, cleanup: owner.cleanup })),
  };
}
