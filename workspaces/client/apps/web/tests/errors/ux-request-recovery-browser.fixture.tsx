import type { Agent, Project } from "../../src/types.ts";
import type { GetResult } from "../../src/apiTypes.ts";
declare global {
  interface Window {
    uxRequestRecoveryBrowserSelected: string[];
    uxRequestRecoveryBrowserSaves: number;
    uxRequestRecoveryBrowserFailRefresh: boolean;
  }
}
import { required } from "../teams/helpers.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Picker from "../../src/components/ProjectDirectoryPicker.tsx";
import { ClaudeSettings } from "../../src/components/ClaudeSettings.tsx";
import {
  ProjectNameForm,
  MoveChatForm,
} from "../../src/components/ProjectOrganization.tsx";
import ProjectAccount from "../../src/components/ProjectAccount.tsx";
import {
  useAccounts,
  AccountTransferStatus,
} from "../../src/components/Accounts.tsx";
const mode = new URLSearchParams(location.search).get("case") || "";
const project: Project = {
  id: "/project",
  path: "/project",
  name: "Project",
  accountKey: "ready",
  accountKeys: ["ready", "expired"],
  folders: [{ id: "review", name: "Review" }],
};
const agent: Agent = {
  id: "lead",
  rootId: "lead",
  name: "Lead",
  provider: "claude",
  accountKey: "ready",
  status: "idle",
  model: "claude-opus-4-6",
  projectFolder: null,
  projectFolderRevision: 0,
};
const account = (
  id: string,
  label: string,
  status: GetResult<"/api/accounts">["accounts"][number]["status"],
): GetResult<"/api/accounts">["accounts"][number] => ({
  id,
  label,
  status,
  provider: "codex",
  home: "/home/fixture/" + id,
  source: "Codex CLI",
});
const accounts: GetResult<"/api/accounts"> = {
  accounts: [
    account("ready", "Ready account", "ready"),
    account("expired", "Expired account", "signedOut"),
  ],
  archivedAccounts: [],
  logins: [],
  supportsDisconnect: true,
  supportsDelete: true,
  defaultAccountGroups: [],
  defaultAccountKey: "ready",
};
window.uxRequestRecoveryBrowserSelected = [];
window.uxRequestRecoveryBrowserSaves = 0;
window.uxRequestRecoveryBrowserFailRefresh = mode === "confirmed";
const saved = async () => {
  window.uxRequestRecoveryBrowserSaves++;
  if (window.uxRequestRecoveryBrowserFailRefresh) {
    window.uxRequestRecoveryBrowserFailRefresh = false;
    throw Error("Snapshot unavailable");
  }
};
function AccountsFixture() {
  const state = useAccounts("/fixture");
  return (
    <>
      <button onClick={() => state.refresh()}>Refresh accounts</button>
      <button
        onClick={() =>
          state.setData({
            accounts: [account("ready", "New account", "ready")],
            archivedAccounts: [],
            logins: [],
            supportsDisconnect: true,
            supportsDelete: true,
            defaultAccountGroups: [],
            defaultAccountKey: "ready",
          })
        }
      >
        Use saved accounts
      </button>
      <output>{state.data.accounts.map((a) => a.label).join(",")}</output>
    </>
  );
}
let body =
  mode === "picker" ? (
    <Picker
      initialPath="/project"
      onSelect={async (path) => {
        window.uxRequestRecoveryBrowserSelected.push(path);
      }}
    />
  ) : mode === "claude" || mode === "deadline" ? (
    <ClaudeSettings agent={agent} />
  ) : mode.startsWith("accounts") ? (
    <AccountsFixture />
  ) : ["name", "confirmed", "rejected", "project-deadline"].includes(mode) ? (
    <ProjectNameForm project={project} folder="new" saved={saved} />
  ) : mode === "move" ? (
    <MoveChatForm project={project} agent={agent} saved={saved} />
  ) : mode === "project-account" || mode === "unready" ? (
    <ProjectAccount
      path={project.path || "/project"}
      project={
        mode === "unready" ? { ...project, accountKey: "expired" } : project
      }
      defaultAccountKey="ready"
      accounts={accounts}
      saved={saved}
    />
  ) : (
    <AccountTransferStatus
      transfer={{
        status: "pending",
        moved: 1,
        total: 1,
        waitingCount: 0,
        nativeHistoryPending: 1,
      }}
      targetLabel="Ready account"
      onAction={() => {}}
    />
  );
createRoot(required(document.getElementById("root"))).render(
  <MantineProvider>{body}</MantineProvider>,
);
