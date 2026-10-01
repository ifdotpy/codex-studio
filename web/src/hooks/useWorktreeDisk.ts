import { useEffect, useState } from "react";
import { api } from "../api";

export interface WorktreeDiskSnapshot {
  workers: Record<string, { state: string; bytes?: number; scannedAt?: number }>;
  totalBytes: number;
  limitBytes: number;
  warning: boolean;
  scanning: boolean;
  error?: string | null;
}

export function useWorktreeDisk(enabled: boolean) {
  const [disk, setDisk] = useState<WorktreeDiskSnapshot>();
  useEffect(() => {
    if (!enabled) {
      setDisk(undefined);
      return;
    }
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        const result = await api<WorktreeDiskSnapshot>("/api/worktree-disk");
        if (!stopped)
          setDisk((old) =>
            JSON.stringify(old) === JSON.stringify(result) ? old : result,
          );
      } catch {
        // Keep the last measured values during a short network outage.
      } finally {
        if (!stopped) timer = setTimeout(load, 15000);
      }
    };
    void load();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [enabled]);
  return disk;
}
