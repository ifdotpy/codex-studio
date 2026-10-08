import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  choosePlaywrightWorkerCount,
  completeWorkerMemoryRun,
  persistWorkerMemorySample,
  sampleCompetingRunnableProcesses,
  sampleProcessTreeMemory,
} from "../web/playwright-workers.mjs";

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

test("completed worker memory run becomes the persisted latest profile", () => {
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
    completeWorkerMemoryRun(environment);
    const profile = JSON.parse(
      readFileSync(
        join(root, "codex-studio", "playwright-worker-memory", "latest.json"),
        "utf8",
      ),
    );
    assert.equal(profile.processTreePssBytes, 1_100_000_000);
    assert.equal(profile.workerCount, 2);
    assert.equal(profile.processTree, true);
    assert.deepEqual(profile.processTreeBreakdown, [
      { name: "chrome", pssBytes: 700_000_000 },
    ]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("CPU median determines workers when memory has room", () => {
  assert.deepEqual(
    choosePlaywrightWorkerCount({
      cpuLimit: 32,
      competingRunnable: 8,
      memoryWorkers: 32,
    }),
    {
      workers: 24,
      availableCpus: 24,
      cpuFloor: 8,
      bound: "cpu-median",
      memorySafeToStart: true,
    },
  );
});

test("CPU floor prevents a transient runnable spike from selecting two", () => {
  assert.deepEqual(
    choosePlaywrightWorkerCount({
      cpuLimit: 32,
      competingRunnable: 30,
      memoryWorkers: 32,
    }),
    {
      workers: 8,
      availableCpus: 2,
      cpuFloor: 8,
      bound: "cpu-floor",
      memorySafeToStart: true,
    },
  );
});

test("memory remains a hard cap below the CPU floor", () => {
  const selection = choosePlaywrightWorkerCount({
    cpuLimit: 32,
    competingRunnable: 30,
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
