import type { Agent } from "../../src/types.ts";
import { required } from "../teams/helpers.ts";
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import {
  RetainedWorktree,
  TeamRetainedWorktrees,
  useRetainedWorktrees,
} from "../../src/components/agents/RetainedWorktrees.tsx";
const worker: Agent = {
  created: 1,
  updated: 1,
  source: "managed",
  id: "worker",
  worktree: true,
  rootId: "lead",
  status: "completed",
  workspaceCleanupIntent: {
    phase: "not_sent",
    reason:
      "Submodule vendor/documentation has local changes. Cleanup kept the worktree.",
    path: "/Users/example/Projects/studio/.worktrees/codex-agents/worker/vendor/documentation",
  },
};
const lead: Agent = {
  created: 1,
  updated: 1,
  source: "managed",
  id: "lead",
  isLead: true,
};
function Team() {
  const items = useRetainedWorktrees(lead, [worker]);
  return (
    <>
      <TeamRetainedWorktrees items={items} />
      <RetainedWorktree agent={worker} />
    </>
  );
}
createRoot(required(document.getElementById("root"))).render(
  <MantineProvider>
    <main
      style={{
        maxWidth: 900,
        margin: "auto",
        padding: 12,
        display: "grid",
        gap: 12,
      }}
    >
      <h1>Team</h1>
      <Team />
    </main>
  </MantineProvider>,
);
