#!/usr/bin/env node
// Opt-in, isolated 16-worker POST /api/messages ingress acceptance scenario.
import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { performance } from "node:perf_hooks";
import { execFileSync } from "node:child_process";

const harness = dirname(fileURLToPath(import.meta.url));
const parallelWaves = process.argv.includes("--parallel-waves");
const sourceRootAt = process.argv.indexOf("--source-root");
const sourceRoot =
  sourceRootAt >= 0
    ? resolve(process.argv[sourceRootAt + 1])
    : resolve(harness, "../../../../../../../../");
const outputAt = process.argv.indexOf("--output");
const evidenceRoot = join(
  process.env.XDG_STATE_HOME || join(homedir(), ".local", "state"),
  "evidence",
  "latency-components",
);
const outputPath =
  outputAt >= 0
    ? resolve(process.argv[outputAt + 1])
    : join(evidenceRoot, "http-message-acceptance-16.json");
if (outputPath.startsWith(sourceRoot + "/"))
  throw new Error("--output must be outside the checkout");

await mkdir(evidenceRoot, { recursive: true });
const state = await mkdtemp(join(evidenceRoot, ".http-message-acceptance-"));
await mkdir(dirname(outputPath), { recursive: true });
const runStarted = performance.now();
const sourceRevision = execFileSync(
  "git",
  ["-C", sourceRoot, "rev-parse", "HEAD"],
  {
    encoding: "utf8",
  },
).trim();
const measuredSources = [
  "workspaces/runtime/apps/server/src/codex_runtime.py",
  "workspaces/runtime/apps/server/src/studio_api/work/router.py",
  "workspaces/runtime/apps/server/src/studio_api/benchmarks/runtime_load/server.py",
  "workspaces/runtime/apps/server/src/studio_api/benchmarks/runtime_load/message_acceptance.mjs",
];
const sourceDigest = createHash("sha256");
for (const relative of measuredSources) {
  sourceDigest.update(relative);
  sourceDigest.update(await readFile(join(sourceRoot, relative)));
}
const workingTreeDirty = Boolean(
  execFileSync(
    "git",
    ["-C", sourceRoot, "status", "--porcelain", "--untracked-files=all"],
    { encoding: "utf8" },
  ).trim(),
);
const managedPython = execFileSync(
  "python3",
  [join(sourceRoot, "workspaces/runtime/apps/server/src/codex_python.py")],
  { encoding: "utf8" },
).trim();
if (!managedPython)
  throw new Error(
    "codex_python.py did not return the managed interpreter path",
  );
const fixture = spawn(
  managedPython,
  ["-B", join(harness, "server.py"), state, sourceRoot, ""],
  {
    cwd: sourceRoot,
    env: {
      ...process.env,
      BENCH_TEAMS: "1",
      BENCH_WORKERS_PER_TEAM: "16",
      BENCH_STEADY_SECONDS: "0",
      BENCH_ACCOUNT_COUNT: "1",
      BENCH_SQLITE_SYNCHRONOUS: "FULL",
      BENCH_SQLITE_IDLE_CONNECTION: "0",
      BENCH_ENABLE_FAST_DISPATCH: "1",
      BENCH_SOURCE_REVISION: sourceRevision,
    },
    stdio: ["pipe", "pipe", "pipe"],
  },
);

let output = "";
let stderr = "";
let fixtureExit;
let cleanupTimedOut = false;
fixture.stdout.setEncoding("utf8");
fixture.stderr.setEncoding("utf8");
fixture.stdout.on("data", (chunk) => {
  output += chunk;
});
fixture.stderr.on("data", (chunk) => {
  stderr += chunk;
});
fixture.once("exit", (code, signal) => {
  fixtureExit = { code, signal };
});
const hardDeadline = setTimeout(() => {
  cleanupTimedOut = true;
  if (fixture.exitCode === null) fixture.kill("SIGTERM");
  setTimeout(() => {
    if (fixture.exitCode === null) fixture.kill("SIGKILL");
  }, 5_000);
}, 120_000);

const readLines = async (predicate, label, timeoutMs = 30_000) => {
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    const end = output.indexOf("\n");
    if (end >= 0) {
      const line = output.slice(0, end);
      output = output.slice(end + 1);
      if (!line) continue;
      const value = JSON.parse(line);
      if (predicate(value)) return value;
    } else if (fixtureExit) {
      throw new Error(
        `fixture exited ${JSON.stringify(fixtureExit)}: ${stderr}`,
      );
    } else {
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 20));
    }
  }
  throw new Error(`${label} timed out: ${stderr}`);
};

const summarize = (values) => {
  const sorted = [...values].sort((a, b) => a - b);
  const at = (p) => sorted[Math.max(0, Math.ceil(p * sorted.length) - 1)];
  return sorted.length === 0
    ? { samples: 0, p50: null, p95: null, p99: null, max: null }
    : {
        samples: sorted.length,
        p50: at(0.5),
        p95: at(0.95),
        p99: at(0.99),
        max: sorted.at(-1),
      };
};

const json = async (response, label) => {
  let body;
  try {
    body = await response.json();
  } catch {
    throw new Error(`${label} returned non-JSON (HTTP ${response.status})`);
  }
  assert(
    response.ok,
    `${label} failed (HTTP ${response.status}): ${JSON.stringify(body)}`,
  );
  return body;
};

let report;
try {
  const ready = await readLines(
    (line) => line.kind === "ready",
    "fixture startup",
  );
  assert.equal(ready.teams, 1, "fixture must seed one team");
  assert.equal(ready.workersPerTeam, 16, "fixture must seed 16 workers");
  assert.equal(
    ready.workers.length,
    16,
    "fixture must expose all 16 worker IDs",
  );
  assert.equal(
    ready.fastDispatchEnabled,
    true,
    "fixture must enable production fast-dispatch admission",
  );
  assert.equal(
    ready.inertSchedulerStopped,
    true,
    "the inert scheduler must be stopped before admission",
  );
  assert.equal(
    ready.workersInFlight,
    true,
    "all synthetic workers must remain in-flight",
  );

  const session = await json(
    await fetch(`${ready.origin}/api/session`),
    "/api/session",
  );
  assert.equal(typeof session.token, "string");
  const headers = {
    "content-type": "application/json",
    origin: ready.origin,
    "x-canvas-token": session.token,
  };
  const waves = Array.from({ length: 8 }, (_, wave) =>
    ready.workers.map((room) => ({
      id: randomUUID(),
      room,
      text: `runtime-load HTTP acceptance wave ${wave + 1} ${randomUUID()}`,
      delivery: "queue",
      assets: [],
    })),
  );
  const messages = waves.flat();
  const postBatch = async (batch) =>
    Promise.all(
      batch.map(async (body) => {
        const started = performance.now();
        const response = await fetch(`${ready.origin}/api/messages`, {
          method: "POST",
          headers,
          body: JSON.stringify(body),
          signal: AbortSignal.timeout(10_000),
        });
        const receipt = await json(response, "POST /api/messages");
        assert.equal(
          receipt.id,
          body.id,
          "the API must return the exact submitted ID",
        );
        return {
          elapsedMs: performance.now() - started,
          status: response.status,
          receipt,
        };
      }),
    );

  const acceptedWaves = [];
  if (parallelWaves) {
    acceptedWaves.push(await postBatch(messages));
  } else {
    for (const wave of waves) acceptedWaves.push(await postBatch(wave));
  }
  const accepted = acceptedWaves.flat();
  const retryBatch = waves[0];
  const retries = await postBatch(retryBatch);
  const retryIdsPreserved = retries.every(
    (value, index) => value.receipt.id === messages[index].id,
  );
  assert(
    retryIdsPreserved,
    "same-ID retries must return each original message ID",
  );

  const queueViews = await Promise.all(
    ready.workers.map(async (agent) => {
      const url = new URL("/api/queue", ready.origin);
      url.searchParams.set("agent", agent);
      return json(await fetch(url, { headers }), "GET /api/queue");
    }),
  );
  const receiptsByWorker = await Promise.all(
    ready.workers.map(async (agent) => {
      const ids = messages
        .filter((message) => message.room === agent)
        .map((message) => message.id);
      const url = new URL("/api/messages/receipts", ready.origin);
      url.searchParams.set("agent", agent);
      url.searchParams.set("ids", JSON.stringify(ids));
      return json(await fetch(url, { headers }), "GET /api/messages/receipts");
    }),
  );
  const histories = await Promise.all(
    ready.workers.map(async (room) => {
      const url = new URL("/api/messages", ready.origin);
      url.searchParams.set("room", room);
      return json(await fetch(url, { headers }), "GET /api/messages");
    }),
  );
  const oneRuntimeEffectPerId = messages.every((message, index) => {
    const workerIndex = index % ready.workers.length;
    const runtimeMatches = receiptsByWorker[workerIndex].items.filter(
      (item) => item.id === message.id,
    );
    const historyMatches = histories[workerIndex].filter(
      (item) => item.id === message.id,
    );
    return (
      runtimeMatches.length === 1 &&
      ["pending", "reserved", "dispatching"].includes(
        runtimeMatches[0]?.status,
      ) &&
      historyMatches.length === 1
    );
  });
  assert(
    oneRuntimeEffectPerId,
    "each exact message ID must have one durable Runtime event and one persisted chat row",
  );
  assert.equal(
    receiptsByWorker.reduce((sum, value) => sum + value.items.length, 0),
    messages.length,
    "Runtime receipts must contain exactly one event per accepted message ID",
  );

  const diagnosticsDeadline = Date.now() + 20_000;
  let diagnostics;
  do {
    diagnostics = await json(
      await fetch(`${ready.origin}/__bench/diagnostics`),
      "/__bench/diagnostics",
    );
    if (
      diagnostics.activeMethods["POST /api/messages"] === undefined &&
      diagnostics.dispatchQueue.pending === 0 &&
      diagnostics.dispatchQueue.active === 0
    )
      break;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 25));
  } while (Date.now() < diagnosticsDeadline);
  assert.equal(
    diagnostics.activeMethods["POST /api/messages"] || 0,
    0,
    "HTTP acceptance requests must drain",
  );
  assert.equal(
    diagnostics.dispatchQueue.pending,
    0,
    "fast-dispatch submissions must settle",
  );
  assert.equal(
    diagnostics.dispatchQueue.active,
    0,
    "fast-dispatch workers must become idle",
  );
  assert(
    diagnostics.dispatchQueue.submitted > 0,
    "HTTP acceptance must exercise the production fast-dispatch executor",
  );
  assert(
    diagnostics.fakeStartStubCalls > 0,
    "fast dispatch must reach the fake no-native delivery boundary",
  );

  report = {
    schemaVersion: 1,
    benchmark: "runtime_load_http_message_acceptance",
    sourceRevision: ready.sourceRevision,
    sourceDigest: sourceDigest.digest("hex"),
    workingTreeDirty,
    evidenceTier: "local-verified",
    syntheticOnly: true,
    nativeStartStubbed: true,
    inertSchedulerStopped: ready.inertSchedulerStopped,
    workersRemainInFlight: ready.workersInFlight,
    workerCount: ready.workers.length,
    waves: waves.length,
    workersPerWave: ready.workers.length,
    offerMode: parallelWaves ? "parallel_waves" : "sequential_waves",
    concurrentPosts: accepted.length,
    sameIdRetries: retries.length,
    waveLatencyMs: acceptedWaves.map((wave) =>
      summarize(wave.map((item) => item.elapsedMs)),
    ),
    postLatencyMs: summarize(accepted.map((item) => item.elapsedMs)),
    retryLatencyMs: summarize(retries.map((item) => item.elapsedMs)),
    responseStatuses: [
      ...new Set([...accepted, ...retries].map((item) => item.status)),
    ],
    responseStatusCounts: [...accepted, ...retries].reduce((counts, item) => {
      counts[item.status] = (counts[item.status] || 0) + 1;
      return counts;
    }, {}),
    retryIdsPreserved,
    oneRuntimeEffectPerId,
    httpInFlightPeak: diagnostics.peakActiveMethods["POST /api/messages"] || 0,
    httpInFlightAtDrain: diagnostics.activeMethods["POST /api/messages"] || 0,
    serverPostLatencyMs: diagnostics.httpMethodLatencyMs["POST /api/messages"],
    dispatchQueue: diagnostics.dispatchQueue,
    dispatchSubmissionsPerNewMessage:
      diagnostics.dispatchQueue.submitted / messages.length,
    runtimeQueue: {
      pendingItemsAfterDispatch: queueViews.reduce(
        (sum, value) => sum + value.items.length,
        0,
      ),
      reservedEventReceipts: receiptsByWorker.reduce(
        (sum, value) => sum + value.items.length,
        0,
      ),
      eventStatuses: receiptsByWorker
        .flatMap((value) => value.items)
        .reduce((counts, item) => {
          counts[item.status] = (counts[item.status] || 0) + 1;
          return counts;
        }, {}),
      acceptedEventIds: messages.map((message) => message.id),
      activeTurnsWereSyntheticAndRemainInFlight: true,
      drainStatus:
        "HTTP ingress and fast-dispatch executor drained; exact Runtime events remain pending or reserved before the no-native delivery stub.",
    },
    resourceLimits: {
      teams: 1,
      workersPerTeam: 16,
      browserProcesses: 0,
      providerSessions: 0,
      fakeStartStubCalls: diagnostics.fakeStartStubCalls,
      stateProfile: "unique temporary fixture directory",
      sqliteSynchronousMode: ready.sqliteSynchronousMode,
    },
  };
} catch (error) {
  report = {
    schemaVersion: 1,
    benchmark: "runtime_load_http_message_acceptance",
    status: "failed",
    error: String(error?.stack || error),
  };
} finally {
  clearTimeout(hardDeadline);
  if (fixture.exitCode === null) {
    fixture.stdin.write('{"action":"shutdown"}\n');
    fixture.stdin.end();
    await Promise.race([
      new Promise((resolvePromise) => fixture.once("exit", resolvePromise)),
      new Promise((resolvePromise) =>
        setTimeout(() => {
          cleanupTimedOut = true;
          fixture.kill("SIGTERM");
          setTimeout(() => {
            if (fixture.exitCode === null) fixture.kill("SIGKILL");
            if (fixture.exitCode === null) fixture.once("exit", resolvePromise);
            else resolvePromise();
          }, 5_000);
        }, 10_000),
      ),
    ]);
  }
  await rm(state, { recursive: true, force: true });
}

report.fixtureExit = fixtureExit;
report.cleanupTimedOut = cleanupTimedOut;
if (fixtureExit?.code !== 0 && report.status !== "failed") {
  report.status = "failed";
  report.error = `fixture exited ${JSON.stringify(fixtureExit)}: ${stderr}`;
}
report.elapsedMs = Math.round(performance.now() - runStarted);
await writeFile(outputPath, `${JSON.stringify(report, null, 2)}\n`);
console.log(JSON.stringify(report, null, 2));
if (report.status === "failed") process.exitCode = 1;
