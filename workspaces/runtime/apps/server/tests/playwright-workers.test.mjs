import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  choosePlaywrightWorkerCount,
  calculateMemoryBudgetBytes,
  completeWorkerMemoryRun,
  estimateWorkerMemoryBytes,
  persistWorkerMemorySample,
  sampleCompetingRunnableProcesses,
  sampleProcessTreeMemory,
} from "../../../../client/apps/web/playwright-workers.mjs";

test("process-tree sampler sums worker, browser, and fixture descendants", () => {
  const files = {
    "/proc/10/smaps_rollup": "Rss: 100 kB\nPss: 80 kB\n",
    "/proc/10/task/10/children": "11 12",
    "/proc/10/comm": "node\n",
    "/proc/10/cmdline": "node\0playwright-worker\0",
    "/proc/11/smaps_rollup": "Rss: 200 kB\nPss: 150 kB\n",
    "/proc/11/task/11/children": "13",
    "/proc/11/comm": "chrome\n",
    "/proc/11/cmdline": "chromium\0--type=browser\0",
    "/proc/12/smaps_rollup": "Rss: 300 kB\nPss: 250 kB\n",
    "/proc/12/task/12/children": "11",
    "/proc/12/comm": "python3\n",
    "/proc/12/cmdline": "python3\0fixture.py\0",
    "/proc/13/smaps_rollup": "Rss: 400 kB\nPss: 350 kB\n",
    "/proc/13/task/13/children": "",
    "/proc/13/comm": "chrome\n",
    "/proc/13/cmdline": "chromium\0--type=renderer\0",
  };
  const result = sampleProcessTreeMemory(10, (path) => {
    if (!(path in files)) throw new Error("process exited");
    return files[path];
  });
  assert.deepEqual(result, {
    rssBytes: 1000 * 1024,
    pssBytes: 830 * 1024,
    processCount: 4,
    processes: [
      {
        pid: 10,
        rssBytes: 100 * 1024,
        pssBytes: 80 * 1024,
        name: "node",
        command: "node playwright-worker",
      },
      {
        pid: 12,
        rssBytes: 300 * 1024,
        pssBytes: 250 * 1024,
        name: "python3",
        command: "python3 fixture.py",
      },
      {
        pid: 11,
        rssBytes: 200 * 1024,
        pssBytes: 150 * 1024,
        name: "chrome",
        command: "chromium --type=browser",
      },
      {
        pid: 13,
        rssBytes: 400 * 1024,
        pssBytes: 350 * 1024,
        name: "chrome",
        command: "chromium --type=renderer",
      },
    ],
  });
});

test("completed multi-worker run persists p90 process-tree memory profile", () => {
  const root = mkdtempSync(join(tmpdir(), "playwright-memory-profile-"));
  const environment = {
    XDG_STATE_HOME: root,
    CODEX_PLAYWRIGHT_WORKER_SELECTION: JSON.stringify({
      workerMemoryRunId: "run-1",
    }),
  };
  try {
    persistWorkerMemorySample({
      environment,
      runId: "run-1",
      workerPid: 101,
      processTreePssBytes: 900_000_000,
      processTreeBreakdown: [{ name: "node", pssBytes: 500_000_000 }],
    });
    persistWorkerMemorySample({
      environment,
      runId: "run-1",
      workerPid: 102,
      processTreePssBytes: 1_100_000_000,
      processTreeBreakdown: [{ name: "chrome", pssBytes: 700_000_000 }],
    });
    for (let index = 0; index < 6; index += 1)
      persistWorkerMemorySample({
        environment,
        runId: "run-1",
        workerPid: 103 + index,
        processTreePssBytes: 950_000_000 + index * 10_000_000,
        processTreeBreakdown: [{ name: "node", pssBytes: 400_000_000 }],
      });
    completeWorkerMemoryRun(environment);
    const profile = JSON.parse(
      readFileSync(
        join(root, "codex-studio", "playwright-worker-memory", "latest.json"),
        "utf8",
      ),
    );
    assert.equal(profile.processTreePssBytes, 1_100_000_000);
    assert.equal(profile.processTreeP90PssBytes, 1_100_000_000);
    assert.equal(profile.workerCount, 8);
    assert.deepEqual(
      profile.processTreePssSamples,
      [
        900_000_000, 950_000_000, 960_000_000, 970_000_000, 980_000_000,
        990_000_000, 1_000_000_000, 1_100_000_000,
      ],
    );
    assert.equal(profile.processTree, true);
    assert.deepEqual(profile.processTreeBreakdown, [
      { name: "chrome", pssBytes: 700_000_000 },
    ]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("worker memory uses measured p90 or conservative fallback without double-counting tree children", () => {
  const measured = estimateWorkerMemoryBytes({
    profile: { processTreeP90PssBytes: 1_000_000_000, workerCount: 5 },
    fallbackPssBytes: 2_150_000_000,
  });
  assert.equal(measured, 1_000_000_000);
  const fallback = estimateWorkerMemoryBytes({
    profile: { processTreeP90PssBytes: 1_000_000_000, workerCount: 4 },
    fallbackPssBytes: Math.ceil(2.1 * 1024 ** 3),
  });
  assert.equal(fallback, Math.ceil(2.1 * 1024 ** 3));
  assert.equal(
    calculateMemoryBudgetBytes(24 * 1024 ** 3),
    Math.floor(0.7 * 24 * 1024 ** 3),
  );
  assert.equal(calculateMemoryBudgetBytes(5 * 1024 ** 3), 1024 ** 3);
  assert.equal(
    Math.floor(calculateMemoryBudgetBytes(24 * 1024 ** 3) / fallback),
    7,
  );
});

test("CPU budget plans six workers on an idle 32-CPU host", () => {
  assert.deepEqual(
    choosePlaywrightWorkerCount({
      cpuLimit: 32,
      competingRunnable: 0,
      memoryWorkers: 32,
    }),
    {
      workers: 6,
      availableCpus: 32,
      cpuBudgetCores: 9,
      cpuWorkerSlots: 6,
      measuredWorkerCpuCores: 0.54,
      workerCpuSafetyFactor: 2.5,
      maximumWorkerCpuFraction: 0.3,
      plannedWorkerCpuCores: 1.35,
      bound: "cpu-capacity",
      memorySafeToStart: true,
    },
  );
});

test("competing runnable work reduces the automatic worker plan", () => {
  const selection = choosePlaywrightWorkerCount({
    cpuLimit: 32,
    competingRunnable: 8,
    memoryWorkers: 32,
  });
  assert.equal(selection.workers, 5);
  assert.equal(selection.availableCpus, 24);
  assert.equal(selection.cpuBudgetCores, 7);
  assert.equal(selection.maximumWorkerCpuFraction, 0.3);
});

test("the available CPU count has a one-worker floor", () => {
  assert.deepEqual(
    choosePlaywrightWorkerCount({
      cpuLimit: 32,
      competingRunnable: 30,
      memoryWorkers: 32,
    }),
    {
      workers: 1,
      availableCpus: 2,
      cpuBudgetCores: 1,
      cpuWorkerSlots: 1,
      measuredWorkerCpuCores: 0.54,
      workerCpuSafetyFactor: 2.5,
      maximumWorkerCpuFraction: 0.3,
      plannedWorkerCpuCores: 1.35,
      bound: "cpu-capacity",
      memorySafeToStart: true,
    },
  );
});

test("memory bound takes priority below the CPU capacity plan", () => {
  const selection = choosePlaywrightWorkerCount({
    cpuLimit: 32,
    competingRunnable: 8,
    memoryWorkers: 3,
  });
  assert.equal(selection.workers, 3);
  assert.equal(selection.bound, "memory");
  assert.equal(selection.memorySafeToStart, true);

  const noHeadroom = choosePlaywrightWorkerCount({
    cpuLimit: 32,
    competingRunnable: 0,
    memoryWorkers: 0,
  });
  assert.equal(noHeadroom.workers, 0);
  assert.equal(noHeadroom.bound, "memory");
  assert.equal(noHeadroom.memorySafeToStart, false);
});

test("explicit override is reported and still must fit the memory ceiling", () => {
  const selection = choosePlaywrightWorkerCount({
    cpuLimit: 32,
    competingRunnable: 0,
    memoryWorkers: 3,
    override: "4",
  });
  assert.equal(selection.workers, 4);
  assert.equal(selection.bound, "override");
  assert.equal(selection.memorySafeToStart, false);
});

test("runnable sampling takes the median over eleven samples and excludes self", () => {
  const runningCounts = [1, 2, 3, 4, 5, 6, 7, 8, 29, 30, 31];
  const waits = [];
  const selection = sampleCompetingRunnableProcesses({
    readProcStat: () => `procs_running ${runningCounts.shift()}\n`,
    pause: (milliseconds) => waits.push(milliseconds),
  });

  assert.deepEqual(selection.samples, [0, 1, 2, 3, 4, 5, 6, 7, 28, 29, 30]);
  assert.equal(selection.competingRunnable, 5);
  assert.deepEqual(waits, Array(10).fill(500));
});
