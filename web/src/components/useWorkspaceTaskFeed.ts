import { useEffect, useState } from "react";
import { api, type ApiReadMetadata } from "../api";
import type { BackgroundTask } from "../types";

type TaskFeed = {
  tasks: BackgroundTask[];
  cursor?: { updated: number; id: string };
  reset?: boolean;
  hasMore?: boolean;
  hasMoreChanges?: boolean;
};

export function useWorkspaceTaskFeed(opened: boolean, leadId?: string) {
  const [feed, setFeed] = useState<{
    leadId?: string;
    tasks: BackgroundTask[];
  }>({
    tasks: [],
  });
  useEffect(() => {
    if (!opened || !leadId) return;
    let alive = true;
    let busy = false;
    let cursor: { updated: number; id: string } | undefined;
    let etag: string | undefined;
    let tasks = new Map<string, BackgroundTask>();

    const load = async () => {
      if (!alive || busy) return;
      busy = true;
      try {
        let more = true;
        while (alive && more) {
          const query = new URLSearchParams({ agent: leadId });
          if (cursor !== undefined) query.set("cursor", JSON.stringify(cursor));
          const metadata: ApiReadMetadata = {};
          const result = await api<TaskFeed>(
            `/api/workspace/tasks?${query.toString()}`,
            undefined,
            { etag, readMetadata: metadata },
          );
          etag = metadata.etag || etag;
          if (metadata.notModified) break;
          if (result.reset || cursor === undefined) tasks = new Map();
          for (const task of result.tasks || []) tasks.set(task.id, task);
          cursor = result.cursor;
          more = !!result.hasMoreChanges;
          if (tasks.size > 100) {
            const ordered = [...tasks.values()].sort(
              (a, b) => b.created - a.created,
            );
            tasks = new Map(
              ordered.slice(0, 100).map((task) => [task.id, task]),
            );
          }
          if (alive) setFeed({ leadId, tasks: [...tasks.values()] });
        }
      } catch {
        // The next five-second poll retries from the last applied cursor.
      } finally {
        busy = false;
      }
    };

    void load();
    const timer = setInterval(() => void load(), 5000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [opened, leadId]);

  return feed.leadId === leadId ? feed.tasks : null;
}
