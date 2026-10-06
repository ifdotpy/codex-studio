import { useEffect, useState } from "react";
import type { GetResult } from "../api";
import { watchResourceReads } from "../components/watchResourceReads";
import { getShared } from "../sharedRead";

export type WorktreeDiskSnapshot = GetResult<"/api/worktree-disk">;

export function useWorktreeDisk(
  enabled: boolean,
  workerIds: string[],
  prioritize: boolean,
) {
  const [disk, setDisk] = useState<WorktreeDiskSnapshot>();
  const workerKey = workerIds.join(",");
  useEffect(() => {
    if (!enabled || !workerKey) {
      setDisk(undefined);
      return;
    }
    let stopped = false;
    const resources = workerIds.map((agentId) => ({
      kind: "worktree-disk" as const,
      agentId,
    }));
    const stop = watchResourceReads(
      resources,
      async () => {
        const query =
          prioritize && workerKey ? { workers: workerKey } : undefined;
        const result = await getShared("/api/worktree-disk", { query });
        if (!stopped)
          setDisk((old) =>
            JSON.stringify(old) === JSON.stringify(result) ? old : result,
          );
      },
      () => {
        // Keep the last measured values during a short network outage.
      },
    );
    return () => {
      stopped = true;
      stop();
    };
  }, [enabled, prioritize, workerKey]);
  return disk;
}
