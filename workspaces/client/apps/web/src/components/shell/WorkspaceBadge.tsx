import type { Agent } from "../../types";

export type WorkspaceBadgeInfo = {
  label: "LAYR" | "IMAGE" | "WT" | "SHARED";
  title: string;
};

function workspaceModeInfo(
  mode: NonNullable<Agent["workspaceMode"]>,
): WorkspaceBadgeInfo {
  switch (mode) {
    case "layr":
      return { label: "LAYR", title: "VM with layr" };
    case "image":
      return { label: "IMAGE", title: "Isolated disk image workspace" };
    case "worktree":
      return { label: "WT", title: "Git worktree" };
    case "shared":
      return { label: "SHARED", title: "Shared folder" };
  }
  return mode satisfies never;
}

export function workspaceBadgeInfo(agent?: Agent): WorkspaceBadgeInfo | null {
  if (!agent) return null;
  if (agent.isLead) {
    const mode = agent.workspaceMode;
    return mode && mode !== "shared" ? workspaceModeInfo(mode) : null;
  }
  if (agent.remoteWorker && (!agent.cwd || !agent.workspaceMode)) return null;
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

  const info = workspaceModeInfo(mode);
  const path =
    agent.imageWorkspace && !agent.imageWorkspaceReady
      ? agent.imageWorkspaceRepo || agent.cwd
      : agent.cwd || agent.imageWorkspaceRepo;
  return {
    label: info.label,
    title: `${info.title}${path ? ` · ${path}` : ""}`,
  };
}

export default function WorkspaceBadge({
  agent,
  passive = false,
}: {
  agent?: Agent | undefined;
  passive?: boolean | undefined;
}) {
  const info = workspaceBadgeInfo(agent);
  if (!info) return null;
  return (
    <span
      className="workspace-mode-badge"
      title={info.title}
      aria-label={info.title}
      tabIndex={passive ? undefined : 0}
    >
      {info.label}
    </span>
  );
}
