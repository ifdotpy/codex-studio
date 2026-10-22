import { Button, Modal, Textarea } from "@mantine/core";
import { useState } from "react";
import type { PostBody } from "../../api";
import type { Agent } from "../../types";
import { MAX_MANAGED_MESSAGE_CODEPOINTS } from "../../generated/sharedValues";
import type { ClaudePermissionMode } from "../../generated/claudePermissions";
import {
  WorkerPermissions,
  allowedWorkerModes,
  inheritedClaudeMode,
} from "./WorkerPermissions";

export type WorkerLaunch = {
  id: string;
  profile_id: string;
  name: string;
  provider: string;
  reviewer: boolean;
};

export function WorkerLaunchDialog({
  launch,
  lead,
  busy,
  onClose,
  onStart,
}: {
  launch: WorkerLaunch;
  lead: Agent;
  busy: boolean;
  onClose: () => void;
  onStart: (body: PostBody<"/api/agents">) => Promise<void>;
}) {
  const [prompt, setPrompt] = useState("");
  const [permissionMode, setPermissionMode] = useState<ClaudePermissionMode>();
  const allowed = allowedWorkerModes(lead, launch.reviewer);
  const invalidMode =
    launch.provider === "claude" &&
    (!inheritedClaudeMode(lead) ||
      (permissionMode !== undefined && !allowed.includes(permissionMode)));
  const invalid =
    !prompt.trim() ||
    Array.from(prompt).length > MAX_MANAGED_MESSAGE_CODEPOINTS ||
    invalidMode;
  return (
    <Modal
      opened
      onClose={() => !busy && onClose()}
      title={`Start ${launch.name || "subagent"}`}
      size="lg"
    >
      <form
        className="workspace-form"
        onSubmit={async (event) => {
          event.preventDefault();
          if (invalid) return;
          await onStart({
            id: launch.id,
            name: launch.name,
            profile_id: launch.profile_id,
            prompt,
            parent: lead.id,
            cwd: lead.cwd ?? undefined,
            ...(launch.provider === "claude" &&
            (launch.reviewer || permissionMode)
              ? { permission_mode: launch.reviewer ? "plan" : permissionMode }
              : {}),
          });
        }}
      >
        <p className="workspace-muted">
          Main agent: {lead.name}. The subagent uses this profile's model and
          instructions.
        </p>
        <WorkerPermissions
          lead={lead}
          provider={launch.provider}
          reviewer={launch.reviewer}
          value={permissionMode}
          onChange={setPermissionMode}
        />
        {invalidMode && (
          <p role="alert">
            Select a mode allowed by the lead's current permissions.
          </p>
        )}
        <Textarea
          label="Task for this subagent"
          required
          minRows={5}
          maxLength={MAX_MANAGED_MESSAGE_CODEPOINTS * 2}
          value={prompt}
          onChange={(event) => setPrompt(event.target.value)}
        />
        <Button
          variant="filled"
          type="submit"
          loading={busy}
          disabled={invalid}
        >
          Start subagent
        </Button>
      </form>
    </Modal>
  );
}
