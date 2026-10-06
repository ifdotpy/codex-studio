import { useEffect, useState } from "react";
import { errorText, get, type ApiReadMetadata, type GetResult } from "../api";
import { watchResourceReads } from "./watchResourceReads";

type TaskFeed = GetResult<"/api/workspace/tasks">;

export function useWorkspaceTaskFeed(opened: boolean, leadId?: string) {
  const [feed, setFeed] = useState<{
    leadId?: string;
    tasks: TaskFeed["tasks"];
    error: string;
  }>({
    tasks: [],
    error: "",
  });
  useEffect(() => {
    if (!opened || !leadId) return;
    let alive = true;
    let busy = false;
    let cursor: { updated: number; id: string } | undefined;
    let etag: string | undefined;
    let tasks = new Map<string, TaskFeed["tasks"][number]>();

    const load = async () => {
      if (!alive || busy) return;
      busy = true;
      try {
        let more = true;
        while (alive && more) {
          const query = {
            agent: leadId,
            ...(cursor === undefined ? {} : { cursor: JSON.stringify(cursor) }),
          };
          const metadata: ApiReadMetadata = {};
          const result = await get("/api/workspace/tasks", {
            query,
            etag,
            readMetadata: metadata,
          });
          etag = metadata.etag || etag;
          if (metadata.notModified || result === undefined) break;
          if (result.reset || cursor === undefined) tasks = new Map();
          for (const task of result.tasks) tasks.set(task.id, task);
          cursor = result.cursor ?? undefined;
          more = !!result.hasMoreChanges;
          if (tasks.size > 100) {
            const ordered = [...tasks.values()].sort(
              (a, b) => b.created - a.created,
            );
            tasks = new Map(
              ordered.slice(0, 100).map((task) => [task.id, task]),
            );
          }
          if (alive) setFeed({ leadId, tasks: [...tasks.values()], error: "" });
        }
      } catch (error) {
        if (alive)
          setFeed((previous) => ({
            leadId,
            tasks: previous.leadId === leadId ? previous.tasks : [],
            error: errorText(error),
          }));
      } finally {
        busy = false;
      }
    };

    const stop = watchResourceReads(
      { kind: "tasks", agentId: leadId },
      load,
      (error) => {
        if (alive)
          setFeed((previous) => ({
            leadId,
            tasks: previous.leadId === leadId ? previous.tasks : [],
            error: errorText(error),
          }));
      },
    );
    return () => {
      alive = false;
      stop();
    };
  }, [opened, leadId]);

  return feed.leadId === leadId
    ? { tasks: feed.tasks, error: feed.error }
    : null;
}
