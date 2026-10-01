import { useEffect, useState } from "react";
import { api } from "../api";

export interface WorktreeDiskSnapshot {
  workers: Record<
    string,
    { state: string; bytes?: number; scannedAt?: number; measure?: string }
  >;
  totalBytes: number;
  limitBytes: number;
  measure?: string;
  warning: boolean;
  scanning: boolean;
  error?: string | null;
}

export function useWorktreeDisk(
  enabled: boolean,
  workerIds: string[],
  prioritize: boolean,
) {
  const [disk, setDisk] = useState<WorktreeDiskSnapshot>();
  const workerKey = workerIds.join(",");
  useEffect(() => {
    if (!enabled) {
      setDisk(undefined);
      return;
    }
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    let priorityPending = prioritize && !!workerKey;
    const load = async () => {
      try {
        const query = priorityPending
          ? `?workers=${encodeURIComponent(workerKey)}`
          : "";
        priorityPending = false;
        const result = await api<WorktreeDiskSnapshot>(
          `/api/worktree-disk${query}`,
        );
        if (!stopped)
          setDisk((old) =>
            JSON.stringify(old) === JSON.stringify(result) ? old : result,
          );
      } catch {
        // Keep the last measured values during a short network outage.
        if (prioritize && workerKey) priorityPending = true;
      } finally {
        if (!stopped) timer = setTimeout(load, 15000);
      }
    };
    timer = setTimeout(() => void load(), prioritize ? 250 : 0);
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [enabled, prioritize, workerKey]);
  return disk;
}
