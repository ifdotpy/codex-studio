export function availablePlaywrightWorkers(
  environment?: NodeJS.ProcessEnv,
): number;
export function getPlaywrightWorkerSelection(environment?: NodeJS.ProcessEnv): {
  workers: number;
  bound: "cpu-median" | "cpu-floor" | "memory" | "override";
  overridden: boolean;
  cpus: number;
  runnableSamples: number[];
  competingRunnable: number;
  availableCpus: number;
  cpuFloor: number;
  memoryAvailableBytes: number;
  memoryBudgetBytes: number;
  memoryWorkers: number;
  minimumAvailableMemoryReserveBytes: number;
  maximumMemoryFraction: number;
  minimumCpuWorkerFraction: number;
  measuredWorkerPssBytes: number;
  measuredFixtureScratchBytes: number;
  measuredWorkerMemoryBytes: number;
  memorySafeToStart: boolean;
  workerMemoryProfileRunId?: string;
  workerMemoryProfileSamples: number;
  cached?: boolean;
};
export const measuredFixtureScratchBytes: number;
export function calculateMemoryBudgetBytes(memoryAvailableBytes: number): number;
export function choosePlaywrightWorkerCount(input: {
  cpuLimit: number;
  competingRunnable: number;
  memoryWorkers: number;
  override?: string;
}): {
  workers: number;
  availableCpus: number;
  cpuFloor: number;
  bound: "cpu-median" | "cpu-floor" | "memory" | "override";
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
