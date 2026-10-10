interface ScratchProbe {
  parent: string;
  passed: boolean;
  availableBytes?: number;
  memoryBacked?: boolean;
  mount?: string;
  writeFsyncMs?: number;
  inheritedProbe?: boolean;
  reason?: string;
}

export interface PlaywrightScratchRoot {
  root: string;
  ownedRoot: boolean;
  memoryBacked: boolean;
  mount: string | undefined;
  availableBytes: number;
  probeBytes: number;
  writeFsyncMs: number;
  inheritedProbe: boolean;
  tested: ScratchProbe[];
}

export function choosePlaywrightScratchRoot(
  environment?: NodeJS.ProcessEnv,
  workerCount?: number,
): PlaywrightScratchRoot;
