import { AgentRole as GeneratedApiAgentRole } from "../../generated/api.ts";
import type { Agent } from "../../types";

export type WorkspaceBadgeInfo = {
  label: "Layr" | "Layr VM" | "WT" | "SHARED";
  title: string;
};

export function workspaceBadgeInfo(agent?: Agent): WorkspaceBadgeInfo | null {
  if (!agent) return null;
  if (agent.remoteWorker && (!agent.cwd || !agent.workspaceMode)) return null;
  // Legacy rows without a mode keep only the inference their flags prove.
  const mode =
    agent.workspaceMode ??
    (agent.isLead
      ? undefined
      : agent.worktree === true
        ? "worktree"
        : agent.role === GeneratedApiAgentRole.reviewer ||
            agent.worktree === false
          ? "shared"
          : undefined);
  // A chat lead never shows the shared mode of its own folder.
  if (!mode || mode === "image" || (agent.isLead && mode === "shared"))
    return null;
  const info: WorkspaceBadgeInfo =
    mode === "layr-native"
      ? { label: "Layr", title: "Native Layr workspace" }
      : mode === "layr"
        ? { label: "Layr VM", title: "Virtual machine with Layr" }
        : mode === "worktree"
          ? { label: "WT", title: "Git worktree" }
          : { label: "SHARED", title: "Shared folder" };
  if (!agent.isLead && agent.cwd) info.title += ` · ${agent.cwd}`;
  return info;
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
