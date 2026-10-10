import type { Agent } from "../../types";

export type WorkspaceBadgeInfo = {
  label: "LAYR" | "ASIF" | "WT" | "SHARED";
  title: string;
};

export function workspaceBadgeInfo(agent?: Agent): WorkspaceBadgeInfo | null {
  if (!agent) return null;
  if (agent.isLead) {
    const mode = (agent as { workspaceMode?: string }).workspaceMode;
    if (!mode) return null;
    const label =
      mode === "layr"
        ? "LAYR"
        : mode === "image"
          ? "ASIF"
          : mode === "worktree"
            ? "WT"
            : undefined;
    return label
      ? {
          label,
          title:
            label === "LAYR"
              ? "Linux VM with layr"
              : label === "ASIF"
                ? "Apple Sparse Image Format workspace"
                : "Git worktree",
        }
      : null;
  }
  if (agent.remoteWorker && (!agent.cwd || !agent.workspaceMode)) return null;
  const mode: string | undefined =
    agent.workspaceMode ??
    (agent.imageWorkspace
      ? "image"
      : agent.worktree === true
        ? "worktree"
        : agent.role === "reviewer" || agent.worktree === false
          ? "shared"
          : undefined);
  if (!mode) return null;

  const label =
    mode === "layr"
      ? "LAYR"
      : mode === "image"
        ? "ASIF"
        : mode === "worktree"
          ? "WT"
          : "SHARED";
  const name =
    label === "LAYR"
      ? "Linux VM with layr"
      : label === "ASIF"
        ? "Apple Sparse Image Format workspace"
        : label === "WT"
          ? "Git worktree"
          : "Shared folder";
  const path =
    agent.imageWorkspace && !agent.imageWorkspaceReady
      ? agent.imageWorkspaceRepo || agent.cwd
      : agent.cwd || agent.imageWorkspaceRepo;
  return {
    label,
    title: `${name}${path ? ` · ${path}` : ""}`,
  };
}

export default function WorkspaceBadge({ agent }: { agent?: Agent }) {
  const info = workspaceBadgeInfo(agent);
  if (!info) return null;
  return (
    <span
      className="workspace-mode-badge"
      title={info.title}
      aria-label={info.title}
      tabIndex={0}
    >
      {info.label}
    </span>
  );
}
