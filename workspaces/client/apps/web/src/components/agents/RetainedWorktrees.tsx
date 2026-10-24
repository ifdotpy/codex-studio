import { useEffect, useState } from "react";
import { Button } from "@mantine/core";
import { errorText, type GetResult } from "../../api";
import type { Agent } from "../../types";
import {
  useServerApi,
  useServerFileDialogs,
  useServerResourceReads,
} from "../../servers/ServerServicesContext";
import "./retained-worktrees.css";

type RetainedItem =
  GetResult<"/api/retained-worktrees">["retainedWorktrees"][number];

/** The folder that cleanup kept for one worker, with its reason and path. */
export function RetainedWorktree({ agent }: { agent: Agent }) {
  const { canRevealPath, revealPath } = useServerFileDialogs();
  const [error, setError] = useState("");
  const cleanup = agent.workspaceCleanupIntent;
  const canOpen = !agent.remoteWorker && canRevealPath();
  if (!cleanup || !["not_sent", "unknown"].includes(cleanup.phase)) return null;
  return (
    <section className="retained-worktree" aria-label="Retained worktree">
      <p>
        <strong>
          {cleanup.phase === "unknown"
            ? "Cleanup outcome unconfirmed"
            : agent.worktree || agent.workspaceMode === "worktree"
              ? "Worktree retained"
              : "Workspace retained"}
        </strong>
      </p>
      <p>
        {cleanup.reason ||
          "Cleanup did not confirm removal. Inspect the folder before another cleanup."}
      </p>
      {cleanup.path && <p className="retained-worktree-path">{cleanup.path}</p>}
      <Button
        size="compact-sm"
        disabled={!cleanup.path || !canOpen}
        onClick={async () => {
          setError("");
          try {
            if (cleanup.path) await revealPath(cleanup.path);
          } catch (failure) {
            setError(errorText(failure));
          }
        }}
      >
        Open folder
      </Button>
      {!canOpen && <p>Open this path on the server that owns the worktree.</p>}
      {error && <p role="alert">{error}</p>}
    </section>
  );
}

/**
 * The retained folders of deleted workers of this team. Cleanup can end after a worker left
 * the live team list, so the server reads the stored records; a resource update reads again.
 */
export function useRetainedWorktrees(
  lead: Agent | undefined,
  members: Agent[],
): RetainedItem[] {
  const { get } = useServerApi();
  const watchResourceReads = useServerResourceReads();
  const [items, setItems] = useState<RetainedItem[]>([]);
  const leadId = lead?.id;
  useEffect(() => {
    setItems([]);
    if (!leadId) return;
    let current = true;
    const stop = watchResourceReads(
      { kind: "costs" },
      async () => {
        const value = await get("/api/retained-worktrees", {
          query: { agent: leadId },
        });
        // A paired server of another build may answer without the list; a missing list
        // must not break the screen.
        if (current)
          setItems(
            Array.isArray(value?.retainedWorktrees)
              ? value.retainedWorktrees
              : [],
          );
      },
      () => {
        if (current) setItems([]);
      },
    );
    return () => {
      current = false;
      stop();
    };
  }, [leadId, get, watchResourceReads]);
  return items.filter(
    (item) => !members.some((member) => member.id === item.id),
  );
}

export function TeamRetainedWorktrees({ items }: { items: RetainedItem[] }) {
  return items.map((item) => (
    <div key={item.id}>
      <p>{item.name || item.id}</p>
      <RetainedWorktree
        agent={{
          id: item.id,
          name: item.name,
          worktree: true,
          workspaceCleanupIntent: item.cleanup,
        }}
      />
    </div>
  ));
}
