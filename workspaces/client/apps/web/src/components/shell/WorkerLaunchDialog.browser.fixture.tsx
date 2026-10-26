import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import { WorkerLaunchDialog } from "./WorkerLaunchDialog.tsx";
import type { PostBody } from "../../api.ts";
import type { Agent } from "../../types.ts";
import type { ClaudePermissionMode } from "../../generated/claudePermissions.ts";
import "@mantine/core/styles.css";
import "./Workspace.css";

declare global {
  interface Window {
    workerLaunchFixture: {
      submitted: PostBody<"/api/agents"> | null;
      launch: (
        mode?: ClaudePermissionMode,
        provider?: string,
        reviewer?: boolean,
        yolo?: boolean,
      ) => void;
    };
  }
}
const element = document.getElementById("root");
if (!element) throw new Error("Missing worker launch root");
const root = createRoot(element);
window.workerLaunchFixture = {
  submitted: null,
  launch(mode = "auto", provider = "claude", reviewer = false, yolo = false) {
    window.workerLaunchFixture.submitted = null;
    const lead = {
      id: "lead-id",
      name: "Lead",
      cwd: "/fixture",
      provider: "claude",
      yoloMode: yolo,
      claudeOptions: { permissionMode: mode },
    } satisfies Agent;
    root.render(
      <MantineProvider>
        <WorkerLaunchDialog
          key={mode + provider + reviewer + yolo}
          launch={{
            id: "worker-id",
            profile_id: "profile-id",
            name: "Research worker",
            provider,
            reviewer,
          }}
          lead={lead}
          busy={false}
          onClose={() => {}}
          onStart={async (body) => {
            window.workerLaunchFixture.submitted = body;
          }}
        />
      </MantineProvider>,
    );
  },
};
window.workerLaunchFixture.launch();
