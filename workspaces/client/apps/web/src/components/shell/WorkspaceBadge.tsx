import type { Agent } from "../../types";

export type WorkspaceBadgeInfo = {
  label: "Layr" | "Layr VM" | "WT" | "SHARED";
  title: string;
};

export function workspaceBadgeInfo(agent?: Agent): WorkspaceBadgeInfo | null {
  if (!agent) return null;
  if (agent.remoteWorker && (!agent.cwd || !agent.workspaceMode)) return null;
  const mode =
    agent.workspaceMode ??
    (agent.isLead ? undefined : agent.worktree ? "worktree" : "shared");
  if (!mode || mode === "image") return null;
  const info: WorkspaceBadgeInfo =
    mode === "layr-native"
      ? { label: "Layr", title: "Native Layr workspace" }
      : mode === "layr"
        ? { label: "Layr VM", title: "Linux VM with Layr" }
        : mode === "worktree"
          ? { label: "WT", title: "Git worktree" }
          : { label: "SHARED", title: "Shared folder" };
  if (!agent.isLead && agent.cwd) info.title += ` · ${agent.cwd}`;
  return info;
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
