import { readFileSync } from "node:fs";
import { mkdirSync, readdirSync, renameSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import os from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const defaultWorkerTreePssBytes = Math.ceil(2.1 * 1024 ** 3);
export const measuredFixtureScratchBytes = 18 * 1024 * 1024;
const minimumWorkerMemoryProfileSamples = 5;
const maximumMemoryFraction = 0.7;
const minimumAvailableMemoryReserveBytes = 4 * 1024 * 1024 * 1024;
const minimumCpuWorkerFraction = 0.25;
const runnableSampleCount = 11;
const runnableSampleIntervalMs = 500;

function memoryStateDirectory(environment = process.env) {
  return join(
    environment.XDG_STATE_HOME || join(os.homedir(), ".local", "state"),
    "codex-studio",
    "playwright-worker-memory",
  );
}

export function sampleProcessTreeMemory(
  rootPid,
  readProcFile = (path) => readFileSync(path, "utf8"),
) {
  const pending = [String(rootPid)];
  const seen = new Set();
  let rssBytes = 0;
  let pssBytes = 0;
  const processes = [];
  while (pending.length) {
    const pid = pending.pop();
    if (seen.has(pid)) continue;
    seen.add(pid);
    try {
      const rollup = readProcFile(`/proc/${pid}/smaps_rollup`);
      const rssKb = Number(rollup.match(/^Rss:\s+(\d+) kB$/m)?.[1]);
      const pssKb = Number(rollup.match(/^Pss:\s+(\d+) kB$/m)?.[1]);
      if (Number.isFinite(rssKb) && Number.isFinite(pssKb)) {
        const process = {
          pid: Number(pid),
          rssBytes: rssKb * 1024,
          pssBytes: pssKb * 1024,
          name: "unknown",
          command: "",
        };
        try {
          process.name = readProcFile(`/proc/${pid}/comm`).trim();
        } catch {
          // Process may exit between smaps and comm reads.
        }
        try {
          process.command = readProcFile(`/proc/${pid}/cmdline`)
            .replace(/\0/g, " ")
            .trim();
        } catch {
          // Keep the comm label when cmdline is unavailable.
        }
        processes.push(process);
        rssBytes += process.rssBytes;
        pssBytes += process.pssBytes;
      }
    } catch {
      // A short-lived descendant may exit between reading children and smaps.
    }
    try {
      const children = readProcFile(`/proc/${pid}/task/${pid}/children`)
        .trim()
        .split(/\s+/)
        .filter(Boolean);
      pending.push(...children);
    } catch {
      // Not every process exposes children (for example after it exits).
    }
  }
  return { rssBytes, pssBytes, processCount: seen.size, processes };
}

export function persistWorkerMemorySample({
  environment = process.env,
  runId,
  workerPid,
  processTreePssBytes,
  processTreeBreakdown,
}) {
  if (
    !runId ||
    !Number.isSafeInteger(processTreePssBytes) ||
    processTreePssBytes <= 0
  )
    return;
  const directory = join(memoryStateDirectory(environment), "runs", runId);
  mkdirSync(directory, { recursive: true });
  writeFileSync(
    join(directory, `worker-${workerPid}.json`),
    JSON.stringify({
      workerPid,
      processTree: true,
      processTreePssBytes,
      processTreeBreakdown,
    }),
  );
}

export function estimateWorkerMemoryBytes({ profile, fallbackPssBytes }) {
  return Number.isSafeInteger(profile?.processTreeP90PssBytes) &&
    profile.processTreeP90PssBytes > 0 &&
    Number.isSafeInteger(profile.workerCount) &&
    profile.workerCount >= minimumWorkerMemoryProfileSamples
    ? profile.processTreeP90PssBytes
    : fallbackPssBytes;
}

export function calculateMemoryBudgetBytes(memoryAvailableBytes) {
  return Math.min(
    Math.floor(memoryAvailableBytes * maximumMemoryFraction),
    Math.max(0, memoryAvailableBytes - minimumAvailableMemoryReserveBytes),
  );
}

export function completeWorkerMemoryRun(environment = process.env) {
  let selection;
  try {
    selection = JSON.parse(
      environment.CODEX_PLAYWRIGHT_WORKER_SELECTION || "{}",
    );
  } catch {
    return;
  }
  const runId = selection.workerMemoryRunId;
  if (typeof runId !== "string") return;
  const stateDirectory = memoryStateDirectory(environment);
  const runDirectory = join(stateDirectory, "runs", runId);
  let samples = [];
  try {
    samples = readdirSync(runDirectory)
      .filter((name) => /^worker-\d+\.json$/.test(name))
      .map((name) => JSON.parse(readFileSync(join(runDirectory, name), "utf8")))
      .filter(
        (entry) =>
          entry.processTree === true &&
          Number.isSafeInteger(entry.processTreePssBytes) &&
          entry.processTreePssBytes > 0,
      );
  } catch {
    return;
  }
  if (samples.length < minimumWorkerMemoryProfileSamples) return;
  const orderedPssBytes = samples
    .map((entry) => entry.processTreePssBytes)
    .sort((left, right) => left - right);
  const p90Index = Math.max(0, Math.ceil(orderedPssBytes.length * 0.9) - 1);
  const p90PssBytes = orderedPssBytes[p90Index];
  const profile = {
    runId,
    completedAt: Date.now(),
    processTree: true,
    processTreePssBytes: p90PssBytes,
    processTreeP90PssBytes: p90PssBytes,
    processTreePssSamples: orderedPssBytes,
    workerCount: samples.length,
    processTreeBreakdown: samples.reduce(
      (largest, entry) =>
        entry.processTreePssBytes > (largest?.processTreePssBytes ?? 0)
          ? {
              processTreePssBytes: entry.processTreePssBytes,
              processes: entry.processTreeBreakdown,
            }
          : largest,
      undefined,
    )?.processes,
  };
  mkdirSync(stateDirectory, { recursive: true });
  const latestPath = join(stateDirectory, "latest.json");
  try {
    const current = JSON.parse(readFileSync(latestPath, "utf8"));
    if (current.completedAt > profile.completedAt) return;
  } catch {
    // No previous completed profile is available.
  }
  const temporaryPath = `${latestPath}.${process.pid}.${randomUUID()}.tmp`;
  writeFileSync(temporaryPath, JSON.stringify(profile));
  renameSync(temporaryPath, latestPath);
}

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
  const memoryBudget = calculateMemoryBudgetBytes(memoryAvailable);
  let memoryProfile;
  try {
    memoryProfile = JSON.parse(
      readFileSync(
        join(memoryStateDirectory(environment), "latest.json"),
        "utf8",
      ),
    );
  } catch {
    // Use the conservative default until a multi-worker process-tree profile exists.
  }
  const measuredWorkerPssBytes =
    Number.isSafeInteger(memoryProfile?.processTreeP90PssBytes) &&
    Number.isSafeInteger(memoryProfile?.workerCount) &&
    memoryProfile.workerCount >= minimumWorkerMemoryProfileSamples
      ? memoryProfile.processTreeP90PssBytes
      : defaultWorkerTreePssBytes;
  const measuredWorkerMemoryBytes = estimateWorkerMemoryBytes({
    profile: memoryProfile,
    fallbackPssBytes: defaultWorkerTreePssBytes,
  });
  const memoryLimit = Math.floor(memoryBudget / measuredWorkerMemoryBytes);
  const workerMemoryRunId = `${Date.now()}-${process.pid}-${randomUUID()}`;
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
    workerMemoryProfileRunId: memoryProfile?.runId,
    workerMemoryProfileSamples: memoryProfile?.workerCount ?? 0,
    workerMemoryRunId,
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
