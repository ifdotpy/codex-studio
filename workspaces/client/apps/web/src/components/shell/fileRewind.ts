import { providerSupports } from "../../generated/providerKind.ts";
import { WorkspaceMode } from "../../generated/api.ts";

/**
 * The file-rewind panel replaces the Git checkpoint list only when the agent's
 * provider reports the `rewind` capability and its folder is not a worktree.
 */
export function showsFileRewind(agent: {
  provider?: string | null | undefined;
  workspaceMode?: string | null | undefined;
  worktree?: unknown;
}): boolean {
  return (
    providerSupports(agent.provider, "rewind") &&
    (agent.workspaceMode === WorkspaceMode.shared ||
      (!agent.worktree && agent.workspaceMode !== WorkspaceMode.worktree))
  );
}
