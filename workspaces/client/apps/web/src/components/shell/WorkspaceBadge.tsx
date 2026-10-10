import type { Agent } from "../../types";

export type WorkspaceBadgeInfo = {
  label: "ASIF" | "VM" | "WT" | "SHARED";
  title: string;
};

export function workspaceBadgeInfo(agent?: Agent): WorkspaceBadgeInfo | null {
  if (!agent || agent.isLead) return null;
  if (
    agent.remoteWorker &&
    (!agent.cwd ||
      !agent.workspaceMode ||
      (agent.workspaceMode === "image" &&
        !agent.workspaceBackend &&
        !agent.environment))
  )
    return null;
  const mode =
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
    mode === "image"
      ? agent.workspaceBackend === "vm" || agent.environment === "linux"
        ? "VM"
        : "ASIF"
      : mode === "worktree"
        ? "WT"
        : "SHARED";
  const name =
    label === "ASIF"
      ? "Apple Sparse Image Format workspace"
      : label === "VM"
        ? "Linux virtual machine workspace"
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
