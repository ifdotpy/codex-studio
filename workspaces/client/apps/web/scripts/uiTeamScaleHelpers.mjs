import assert from "node:assert/strict";

// Pure helpers of verify-ui-team-scale.mjs. Every value below has one owner:
// this module for the verifier's own pacing and limits, the runtime for the
// spawn batch size (`orchestration_spawn`, docs/orchestration.md).

// Sampling period of the live observation loop, milliseconds.
export const SAMPLE_INTERVAL_MS = 1_000;
// Period of /api/diagnostics snapshots during a run, milliseconds.
export const DIAGNOSTICS_INTERVAL_MS = 5_000;
// Period of the typed-character echo probe, milliseconds.
export const ECHO_PROBE_INTERVAL_MS = 3_000;
// Frame gaps shorter than this are not kept in the evidence, milliseconds.
export const FRAME_GAP_RECORD_MS = 100;
// Largest worker batch of one `orchestration_spawn` call (runtime contract).
export const SPAWN_BATCH_MAX = 64;
// Largest team this verifier accepts (workers, excluding the lead).
export const WORKERS_MAX = 512;
// Seconds the worker's shell command sleeps before it prints its token.
export const WORKER_COMMAND_SECONDS = 3;
// Stage deadline from sending the prompt: a fixed part plus a part per worker.
export const STAGE_DEADLINE_BASE_MS = 120_000;
export const STAGE_DEADLINE_PER_WORKER_MS = 500;
// Wait for the cleanup to be observed (rows deleted, processes gone).
export const CLEANUP_DEADLINE_MS = 120_000;
// Pass limits.
export const FRAME_GAP_LIMIT_MS = 1_000;
export const ECHO_LATENCY_LIMIT_MS = 300;
export const API_P95_LIMIT_MS = 1_000;
// One fully busy core is 100 percent.
const PERCENT = 100;

export const LEAD_PROFILE_NAMES = Object.freeze([
  "codex-luna",
  "claude-sonnet",
]);
export const WORKER_PROFILE_NAMES = Object.freeze([
  "codex-luna",
  "claude-haiku",
]);

export function stageDeadlineMs(workers) {
  return STAGE_DEADLINE_BASE_MS + workers * STAGE_DEADLINE_PER_WORKER_MS;
}

export function percentile(values, fraction) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[
    Math.min(sorted.length - 1, Math.ceil(fraction * sorted.length) - 1)
  ];
}

export function workerToken(runTag, index) {
  return `${runTag}_W${String(index).padStart(4, "0")}`;
}

export function leadDoneToken(runTag) {
  return `${runTag}_ALL_REPLIED`;
}

// One prompt, typed like a user would. The lead gets the token rule instead of
// a list so that the prompt stays short; it spawns the workers in parallel
// batches inside one turn.
export function buildLeadPrompt({ workers, runTag, workspace }) {
  const batches = Math.ceil(workers / SPAWN_BATCH_MAX);
  return [
    `Automated Studio acceptance test ${runTag}. Do not edit files.`,
    `Spawn exactly ${workers} subagents with orchestration_spawn, numbered 1 to ${workers}.`,
    `Use ${batches} call(s) with at most ${SPAWN_BATCH_MAX} agents each; issue all calls in this turn without waiting between them.`,
    `Give every agent workspace "${workspace}", the default model and effort, and this task with N replaced by its number zero-padded to 4 digits:`,
    `"Run exactly one shell command: sleep ${WORKER_COMMAND_SECONDS}; echo ${runTag}_WNNNN. Then reply with only the printed token and stop."`,
    `Spawn nothing else and run no commands yourself. When all ${workers} agents have replied, answer with only ${leadDoneToken(runTag)}.`,
  ].join(" ");
}

// Tracks the agents of one team from entity documents. `observe` takes the
// stored agent value and the sample time (milliseconds since the prompt).
export function createTeamTracker(leadId, runTag, workers) {
  const expected = new Map();
  for (let index = 1; index <= workers; index += 1)
    expected.set(workerToken(runTag, index), index);
  const byId = new Map();
  let leadReportedAtMs = null;
  const text = (value) =>
    `${value.lastAnswer ?? ""}\n${value.overview?.result ?? ""}`;
  return {
    observe(value, atMs) {
      if (value.id === leadId) {
        if (
          leadReportedAtMs === null &&
          text(value).includes(leadDoneToken(runTag))
        )
          leadReportedAtMs = atMs;
        return;
      }
      if (value.rootId !== leadId) return;
      const row = byId.get(value.id) ?? {
        id: value.id,
        firstSeenAtMs: atMs,
        startedAtMs: null,
        completedAtMs: null,
        token: null,
        status: null,
        error: null,
      };
      byId.set(value.id, row);
      row.status = value.status ?? null;
      row.error = value.error ?? null;
      if (row.startedAtMs === null && (value.inFlight || value.turnId))
        row.startedAtMs = atMs;
      if (row.completedAtMs === null) {
        const token = [...expected.keys()].find((item) =>
          text(value).includes(item),
        );
        if (token) {
          row.token = token;
          row.completedAtMs = atMs;
        }
      }
    },
    workers: () => [...byId.values()],
    leadReportedAtMs: () => leadReportedAtMs,
    summary() {
      const rows = [...byId.values()];
      const tokens = new Set(
        rows.filter((row) => row.token).map((row) => row.token),
      );
      return {
        expected: workers,
        visible: rows.length,
        started: rows.filter((row) => row.startedAtMs !== null).length,
        replied: tokens.size,
        missingTokens: [...expected.keys()].filter(
          (token) => !tokens.has(token),
        ),
        leadReportedAtMs,
      };
    },
  };
}

// First sample time at which `count(sample)` reached `target`, or null.
export function firstReachedMs(samples, count, target) {
  const hit = samples.find((sample) => count(sample) >= target);
  return hit ? hit.atMs : null;
}

// CPU milliseconds used between two samples by a set of processes. `previous`
// maps pid to cumulative CPU; a process new in `current` counts in full.
export function cpuDeltaMs(previous, current) {
  let total = 0;
  for (const { pid, cpuMs } of current)
    total += Math.max(0, cpuMs - (previous.get(pid) ?? 0));
  return total;
}

export function cpuPercent(deltaCpuMs, deltaWallMs) {
  return deltaWallMs > 0
    ? Math.round((deltaCpuMs * PERCENT * 10) / deltaWallMs) / 10
    : null;
}

// Terminal SQLite lock errors counted by the backend (diagnostics snapshot).
export function sqliteLockErrors(diagnostics) {
  const count = diagnostics?.sqliteContention?.lockError?.lockErrors;
  return Number.isSafeInteger(count) && count >= 0 ? count : null;
}

export function sqliteWriteWaits(diagnostics) {
  const lock = diagnostics?.sqliteContention?.lockError;
  return {
    count: lock?.writeWaits ?? null,
    totalMs: lock?.writeWaitMs ?? null,
  };
}

// Pass/fail decision. `metrics` holds only measured values; null means "not
// observed", which fails closed.
export function evaluateCriteria(metrics, workers) {
  const checks = [
    ["allAgentsVisibleInUi", metrics.uiAllVisibleMs !== null],
    ["allAgentsReplied", metrics.replied === workers],
    ["leadReported", metrics.leadReportedAtMs !== null],
    ["noSqliteLockErrors", metrics.sqliteLockErrorDelta === 0],
    [
      `frameGapUnder${FRAME_GAP_LIMIT_MS}ms`,
      metrics.maxFrameGapMs !== null &&
        metrics.maxFrameGapMs <= FRAME_GAP_LIMIT_MS,
    ],
    [
      `echoUnder${ECHO_LATENCY_LIMIT_MS}ms`,
      metrics.maxEchoMs !== null && metrics.maxEchoMs <= ECHO_LATENCY_LIMIT_MS,
    ],
    [
      `apiP95Under${API_P95_LIMIT_MS}ms`,
      metrics.apiP95Ms !== null && metrics.apiP95Ms <= API_P95_LIMIT_MS,
    ],
  ];
  return {
    passed: checks.every(([, ok]) => ok),
    checks: Object.fromEntries(checks),
    failures: checks.filter(([, ok]) => !ok).map(([name]) => name),
  };
}

export const USAGE = `Usage: node verify-ui-team-scale.mjs [options]
  --workers N          subagents to spawn, 1..${WORKERS_MAX} (default 3)
  --lead PROFILE       ${LEAD_PROFILE_NAMES.join(" | ")} (default codex-luna)
  --worker PROFILE     ${WORKER_PROFILE_NAMES.join(" | ")} (default codex-luna)
  --worker-workspace   shared | worktree (default shared)
  --project PATH       registered project (default: main checkout of this repository)
  --origin URL         backend (default http://127.0.0.1:4620)
  --evidence-dir DIR   absolute directory outside the checkout (default under ~/.cache)
  --keep               skip the cleanup so the chat can be inspected
Live run against a real backend: announce it. Headless Chromium only.`;

export function parseArgs(argv, defaults) {
  const options = {
    workers: 3,
    lead: "codex-luna",
    worker: "codex-luna",
    workerWorkspace: "shared",
    origin: "http://127.0.0.1:4620",
    keep: false,
    help: false,
    ...defaults,
  };
  const value = (index, name) => {
    const next = argv[index + 1];
    assert.ok(
      next !== undefined && !next.startsWith("--"),
      `${name} needs a value.`,
    );
    return next;
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") options.help = true;
    else if (arg === "--keep") options.keep = true;
    else if (arg === "--workers") options.workers = Number(value(index++, arg));
    else if (arg === "--lead") options.lead = value(index++, arg);
    else if (arg === "--worker") options.worker = value(index++, arg);
    else if (arg === "--worker-workspace")
      options.workerWorkspace = value(index++, arg);
    else if (arg === "--project") options.project = value(index++, arg);
    else if (arg === "--origin") options.origin = value(index++, arg);
    else if (arg === "--evidence-dir")
      options.evidenceDir = value(index++, arg);
    else assert.fail(`Unknown option ${arg}.\n${USAGE}`);
  }
  assert.ok(
    Number.isSafeInteger(options.workers) &&
      options.workers >= 1 &&
      options.workers <= WORKERS_MAX,
    `--workers must be an integer from 1 to ${WORKERS_MAX}.`,
  );
  assert.ok(
    LEAD_PROFILE_NAMES.includes(options.lead),
    `--lead must be one of ${LEAD_PROFILE_NAMES.join(", ")}.`,
  );
  assert.ok(
    WORKER_PROFILE_NAMES.includes(options.worker),
    `--worker must be one of ${WORKER_PROFILE_NAMES.join(", ")}.`,
  );
  return options;
}

export function summaryText(journal) {
  const r = journal.result;
  const m = journal.metrics;
  const line = (label, value) => `${label.padEnd(34)}${value}`;
  return [
    `UI team scale run ${journal.runTag}: ${r.passed ? "PASSED" : "FAILED"}`,
    line(
      "workers / lead / worker profile",
      `${journal.options.workers} / ${journal.options.lead} / ${journal.options.worker}`,
    ),
    line("failures", r.failures.join(", ") || "none"),
    line(
      "first/all agents visible in UI (s)",
      `${sec(m.uiFirstVisibleMs)} / ${sec(m.uiAllVisibleMs)}`,
    ),
    line(
      "first/all agents in backend (s)",
      `${sec(m.backendFirstVisibleMs)} / ${sec(m.backendAllVisibleMs)}`,
    ),
    line(
      "workers started / replied",
      `${m.started} / ${m.replied} of ${journal.options.workers}`,
    ),
    line("lead reported (s)", sec(m.leadReportedAtMs)),
    line(
      "max frame gap / max echo (ms)",
      `${m.maxFrameGapMs} / ${m.maxEchoMs}`,
    ),
    line(
      "long tasks / longest (ms)",
      `${m.longTaskCount} / ${m.longestLongTaskMs}`,
    ),
    line(
      "API p95 (ms) / requests",
      `${m.apiP95Ms} / ${m.apiRequestCount} (diagnostics p95 ${m.diagnosticsP95Ms})`,
    ),
    line(
      "sqlite lock errors / write waits",
      `${m.sqliteLockErrorDelta} / ${m.sqliteWriteWaitDelta}`,
    ),
    line(
      "backend CPU% peak / RSS MiB peak",
      `${m.backendCpuPercentPeak} / ${m.backendRssMiBPeak}`,
    ),
    line(
      "provider CPU% peak / RSS MiB peak",
      `${m.providerCpuPercentPeak} / ${m.providerRssMiBPeak}`,
    ),
    line("cleanup", journal.cleanup?.state ?? "not run"),
    "",
  ].join("\n");
}

function sec(ms) {
  return ms === null || ms === undefined ? "n/a" : (ms / 1000).toFixed(1);
}
