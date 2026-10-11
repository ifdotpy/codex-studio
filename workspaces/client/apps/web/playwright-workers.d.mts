export function darwinAvailableBytes(
  level: string | number,
  totalBytes?: number,
): number | undefined;
export function availablePlaywrightWorkers(
  environment?: NodeJS.ProcessEnv,
): number;
export function getPlaywrightWorkerSelection(environment?: NodeJS.ProcessEnv): {
  workers: number;
  bound: "cpu-capacity" | "memory" | "override";
  overridden: boolean;
  cpus: number;
  runnableSamples: number[];
  competingRunnable: number;
  availableCpus: number;
  cpuBudgetCores: number;
  cpuWorkerSlots: number;
  measuredWorkerCpuCores: number;
  workerCpuSafetyFactor: number;
  maximumWorkerCpuFraction: number;
  plannedWorkerCpuCores: number;
  memoryAvailableBytes: number;
  memoryBudgetBytes: number;
  memoryWorkers: number;
  minimumAvailableMemoryReserveBytes: number;
  maximumMemoryFraction: number;
  measuredWorkerPssBytes: number;
  measuredFixtureScratchBytes: number;
  measuredWorkerMemoryBytes: number;
  memorySafeToStart: boolean;
  workerMemoryProfileRunId?: string;
  workerMemoryProfileSamples: number;
  cached?: boolean;
};
export const measuredFixtureScratchBytes: number;
export function sampleProcessTreeMemory(
  rootPid: number | string,
  readProcFile?: (path: string) => string,
): {
  rssBytes: number;
  pssBytes: number;
  processCount: number;
  processes: Array<{
    pid: number;
    rssBytes: number;
    pssBytes: number;
    name: string;
    command: string;
  }>;
};
export function persistWorkerMemorySample(input: {
  environment?: NodeJS.ProcessEnv;
  runId?: string;
  workerPid: number;
  processTreePssBytes: number;
  processTreeBreakdown: Array<{
    pid: number;
    rssBytes: number;
    pssBytes: number;
    name: string;
    command: string;
  }>;
}): void;
export function calculateMemoryBudgetBytes(
  memoryAvailableBytes: number,
): number;
export function choosePlaywrightWorkerCount(input: {
  cpuLimit: number;
  competingRunnable: number;
  memoryWorkers: number;
  override?: string;
}): {
  workers: number;
  availableCpus: number;
  cpuBudgetCores: number;
  cpuWorkerSlots: number;
  measuredWorkerCpuCores: number;
  workerCpuSafetyFactor: number;
  maximumWorkerCpuFraction: number;
  plannedWorkerCpuCores: number;
  bound: "cpu-capacity" | "memory" | "override";
  memorySafeToStart: boolean;
};
export function estimateWorkerMemoryBytes(input: {
  profile?: {
    processTreeP90PssBytes?: number;
    workerCount?: number;
  };
  fallbackPssBytes: number;
}): number;
export function sampleCompetingRunnableProcesses(input?: {
  readProcStat?: () => string | undefined;
  pause?: (milliseconds: number) => void;
}): { samples: number[]; competingRunnable: number };
