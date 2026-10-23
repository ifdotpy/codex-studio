import { NativeSelect } from "@mantine/core";
import type { Agent } from "../../types";
import {
  CLAUDE_PERMISSION_MODES,
  type ClaudePermissionMode,
} from "../../generated/claudePermissions";

export function inheritedClaudeMode(lead: Agent): ClaudePermissionMode | null {
  if (lead.provider !== "claude")
    return lead.yoloMode !== false ? "bypassPermissions" : "default";
  const mode = lead.claudeOptions?.permissionMode;
  if (mode === undefined)
    return lead.yoloMode === true ? "bypassPermissions" : "default";
  return CLAUDE_PERMISSION_MODES.find((item) => item === mode) ?? null;
}

export function allowedWorkerModes(
  lead: Agent,
  reviewer: boolean,
): ClaudePermissionMode[] {
  const inherited = inheritedClaudeMode(lead);
  if (!inherited) return [];
  if (reviewer) return ["plan"];
  return CLAUDE_PERMISSION_MODES.filter(
    (mode) => mode !== "bypassPermissions" || inherited === "bypassPermissions",
  );
}

export function WorkerPermissions({
  lead,
  provider,
  reviewer,
  value,
  onChange,
}: {
  lead: Agent;
  provider: string;
  reviewer: boolean;
  value: ClaudePermissionMode | undefined;
  onChange: (value: ClaudePermissionMode | undefined) => void;
}) {
  if (provider !== "claude") {
    const sandbox =
      lead.yoloMode !== false
        ? "danger-full-access"
        : reviewer
          ? "read-only"
          : "workspace-write";
    const approval = lead.yoloMode !== false ? "never" : "on-request";
    return (
      <p className="workspace-muted">
        Codex sandbox: {sandbox}. Approval policy: {approval}. These settings
        follow the lead.
      </p>
    );
  }
  const inherited = inheritedClaudeMode(lead);
  const modes = allowedWorkerModes(lead, reviewer);
  return (
    <div>
      <p className="workspace-muted">
        {reviewer
          ? `Lead mode: ${inherited ?? "unknown"}`
          : `Inherits: ${inherited ?? "unknown"} from lead`}
      </p>
      <NativeSelect
        label="Permission mode"
        value={reviewer ? "plan" : (value ?? "inherit")}
        disabled={reviewer || !inherited}
        onChange={(event) =>
          onChange(modes.find((mode) => mode === event.currentTarget.value))
        }
        data={
          reviewer
            ? ["plan"]
            : [
                { value: "inherit", label: "Inherit from lead" },
                ...modes.map((mode) => ({ value: mode, label: mode })),
              ]
        }
        description={
          reviewer
            ? "Reviewers always use plan mode."
            : "Choose a mode or inherit the lead's mode. Bypass requires a lead with bypass permissions."
        }
        error={
          !inherited
            ? "The lead's permission mode is unavailable. Refresh before you start a worker."
            : undefined
        }
      />
    </div>
  );
}
