import assert from "node:assert/strict";
import test from "node:test";

import {
  choosePlaywrightWorkerCount,
  sampleCompetingRunnableProcesses,
} from "../web/playwright-workers.mjs";

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
