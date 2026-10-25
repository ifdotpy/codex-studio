import type { Agent } from "../../types";

export type WorkspaceBadgeInfo = {
  label: "LAYR" | "IMAGE" | "WT" | "SHARED";
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
          ? "IMAGE"
          : mode === "worktree"
            ? "WT"
            : undefined;
    return label
      ? {
          label,
          title:
            label === "LAYR"
              ? "VM with layr"
              : label === "IMAGE"
                ? "Isolated disk image workspace"
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
        ? "IMAGE"
        : mode === "worktree"
          ? "WT"
          : "SHARED";
  const name =
    label === "LAYR"
      ? "VM with layr"
      : label === "IMAGE"
        ? "Isolated disk image workspace"
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
