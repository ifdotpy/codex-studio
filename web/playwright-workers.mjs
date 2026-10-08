import { readFileSync } from "node:fs";
import os from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// A real-spec run with eight workers reached 4.77 GiB peak tree PSS, or
// roughly 0.60 GiB per worker; MemAvailable fell by about 0.75 GiB/worker.
// Use 640 MiB PSS plus a 384 MiB runtime/Vite variance reserve, then add the
// measured 17.64 MiB fixture scratch and round up to a 64 MiB boundary.
const measuredWorkerPssBytes = 640 * 1024 * 1024;
const workerRuntimeReserveBytes = 384 * 1024 * 1024;
export const measuredFixtureScratchBytes = 18 * 1024 * 1024;
const measuredWorkerMemoryBytes =
  Math.ceil(
    (measuredWorkerPssBytes +
      workerRuntimeReserveBytes +
      measuredFixtureScratchBytes) /
      (64 * 1024 * 1024),
  ) *
  64 *
  1024 *
  1024;
const maximumMemoryFraction = 0.5;
const minimumAvailableMemoryReserveBytes = 4 * 1024 * 1024 * 1024;
const minimumCpuWorkerFraction = 0.25;
const runnableSampleCount = 11;
const runnableSampleIntervalMs = 500;

function readText(path) {
  try {
    return readFileSync(path, "utf8").trim();
  } catch {
    return undefined;
  }
}

function cgroupDirectories() {
  const membership = readText("/proc/self/cgroup")
    ?.split("\n")
    .find((line) => line.startsWith("0::"))
    ?.slice(3);
  if (membership === undefined) return ["/sys/fs/cgroup"];
  const directories = [];
  let directory = resolve("/sys/fs/cgroup", `.${membership}`);
  const root = resolve("/sys/fs/cgroup");
  while (directory.startsWith(`${root}/`)) {
    directories.push(directory);
    directory = dirname(directory);
  }
  directories.push(root);
  return directories;
}

function cgroupMemoryAvailableBytes() {
  const headroom = cgroupDirectories()
    .map((directory) => {
      const maximum = readText(join(directory, "memory.max"));
      const current = Number(readText(join(directory, "memory.current")));
      const limit = Number(maximum);
      return maximum &&
        maximum !== "max" &&
        Number.isFinite(current) &&
        Number.isFinite(limit) &&
        limit > 0
        ? Math.max(0, limit - current)
        : undefined;
    })
    .filter((value) => value !== undefined);
  return headroom.length ? Math.min(...headroom) : undefined;
}

function hostMemoryAvailableBytes() {
  const available = readText("/proc/meminfo")
    ?.split("\n")
    .find((line) => line.startsWith("MemAvailable:"))
    ?.match(/\d+/)?.[0];
  const kibibytes = Number(available);
  return Number.isFinite(kibibytes) && kibibytes > 0
    ? kibibytes * 1024
    : os.freemem();
}

function cgroupCpuLimit() {
  const limits = cgroupDirectories()
    .map((directory) => {
      const value = readText(join(directory, "cpu.max"));
      if (!value) return undefined;
      const [quotaText, periodText] = value.split(/\s+/);
      const quota = Number(quotaText);
      const period = Number(periodText);
      if (
        quotaText === "max" ||
        !Number.isFinite(quota) ||
        !Number.isFinite(period) ||
        period <= 0
      )
        return undefined;
      return Math.max(1, Math.floor(quota / period));
    })
    .filter((value) => value !== undefined);
  return limits.length ? Math.min(...limits) : undefined;
}

export function sampleCompetingRunnableProcesses({
  readProcStat = () => readText("/proc/stat"),
  pause = (milliseconds) => {
    const sleeper = new Int32Array(
      new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT),
    );
    Atomics.wait(sleeper, 0, 0, milliseconds);
  },
} = {}) {
  const samples = [];
  for (let index = 0; index < runnableSampleCount; index += 1) {
    if (index > 0) pause(runnableSampleIntervalMs);
    const line = readProcStat()
      ?.split("\n")
      .find((row) => row.startsWith("procs_running "));
    const running = Number(line?.split(/\s+/)[1]);
    // procs_running can include this process while it reads /proc/stat.
    samples.push(
      Number.isSafeInteger(running) && running >= 0
        ? Math.max(0, running - 1)
        : 0,
    );
  }
  const orderedSamples = [...samples].sort((left, right) => left - right);
  return {
    samples,
    competingRunnable:
      orderedSamples[Math.floor(orderedSamples.length / 2)] ?? 0,
  };
}

export function choosePlaywrightWorkerCount({
  cpuLimit,
  competingRunnable,
  memoryWorkers,
  override,
}) {
  const availableCpus = Math.max(1, Math.floor(cpuLimit - competingRunnable));
  const cpuFloor = Math.max(1, Math.ceil(cpuLimit * minimumCpuWorkerFraction));
  const loadLimitedCpus = Math.min(cpuLimit, availableCpus);
  const cpuWorkers = Math.max(cpuFloor, loadLimitedCpus);
  if (override !== undefined) {
    const workers = Number(override);
    if (!Number.isSafeInteger(workers) || workers < 1)
      throw new Error("PLAYWRIGHT_WORKERS must be a positive integer");
    return {
      workers,
      availableCpus,
      cpuFloor,
      bound: "override",
      memorySafeToStart: memoryWorkers > 0 && workers <= memoryWorkers,
    };
  }
  const workers = Math.min(cpuWorkers, memoryWorkers);
  return {
    workers,
    availableCpus,
    cpuFloor,
    bound:
      memoryWorkers < cpuWorkers
        ? "memory"
        : loadLimitedCpus < cpuFloor
          ? "cpu-floor"
          : "cpu-median",
    memorySafeToStart: memoryWorkers > 0,
  };
}

function workerSelection(environment = process.env) {
  const override = environment.PLAYWRIGHT_WORKERS;
  const { samples: runnableSamples, competingRunnable } =
    sampleCompetingRunnableProcesses();
  const cpuLimit = Math.min(
    os.availableParallelism(),
    cgroupCpuLimit() ?? Number.POSITIVE_INFINITY,
  );
  const memoryAvailable = Math.min(
    hostMemoryAvailableBytes(),
    cgroupMemoryAvailableBytes() ?? Number.POSITIVE_INFINITY,
  );
  const memoryBudget = Math.min(
    Math.floor(memoryAvailable * maximumMemoryFraction),
    Math.max(0, memoryAvailable - minimumAvailableMemoryReserveBytes),
  );
  const memoryLimit = Math.floor(memoryBudget / measuredWorkerMemoryBytes);
  const decision = choosePlaywrightWorkerCount({
    cpuLimit,
    competingRunnable,
    memoryWorkers: memoryLimit,
    override,
  });
  return {
    ...decision,
    overridden: override !== undefined,
    cpus: cpuLimit,
    runnableSamples,
    competingRunnable,
    memoryAvailableBytes: memoryAvailable,
    memoryBudgetBytes: memoryBudget,
    memoryWorkers: memoryLimit,
    minimumAvailableMemoryReserveBytes,
    maximumMemoryFraction,
    minimumCpuWorkerFraction,
    measuredWorkerPssBytes,
    workerRuntimeReserveBytes,
    measuredFixtureScratchBytes,
    measuredWorkerMemoryBytes,
  };
}

export function getPlaywrightWorkerSelection(environment = process.env) {
  const cachedSelection = environment.CODEX_PLAYWRIGHT_WORKER_SELECTION;
  if (cachedSelection !== undefined) {
    try {
      const parsed = JSON.parse(cachedSelection);
      if (
        parsed &&
        typeof parsed === "object" &&
        Number.isSafeInteger(parsed.workers) &&
        parsed.workers >= 1 &&
        Number.isSafeInteger(parsed.memoryWorkers) &&
        parsed.memoryWorkers >= parsed.workers
      )
        return { ...parsed, cached: true };
    } catch {
      // Recompute below if an inherited decision was malformed.
    }
  }
  const selection = workerSelection(environment);
  if (!selection.memorySafeToStart)
    throw new Error(
      `Insufficient memory headroom for one Playwright worker: ${selection.memoryAvailableBytes} bytes available, ${selection.minimumAvailableMemoryReserveBytes} bytes reserved, ${selection.measuredWorkerMemoryBytes} bytes measured per worker.`,
    );
  if (selection.workers > selection.memoryWorkers)
    throw new Error(
      `PLAYWRIGHT_WORKERS=${selection.workers} exceeds the current memory ceiling of ${selection.memoryWorkers} workers (${selection.memoryAvailableBytes} bytes available with ${selection.minimumAvailableMemoryReserveBytes} bytes reserved).`,
    );
  environment.CODEX_PLAYWRIGHT_WORKER_SELECTION = JSON.stringify(selection);
  environment.CODEX_PLAYWRIGHT_SELECTED_WORKERS = String(selection.workers);
  return selection;
}

export function availablePlaywrightWorkers(environment = process.env) {
  return getPlaywrightWorkerSelection(environment).workers;
}

if (fileURLToPath(import.meta.url) === resolve(process.argv[1] ?? "")) {
  console.log(JSON.stringify(workerSelection()));
}
