import { lazy, Suspense, useEffect, useState } from "react";
import {
  Button,
  Group,
  Modal,
  SegmentedControl,
  Stack,
  Tabs,
} from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import type { Agent, Snapshot } from "../../types";
import { modalSizes } from "../../theme";
import type { Account, useAccounts } from "../Accounts";
import { UnifiedAgentSettings } from "./UnifiedAgentSettings";
import { ExecutionSettings } from "./ExecutionSettings";
import { AccountExclusionsControl } from "./AccountExclusionsControl";
import SubagentConcurrencyControl from "./SubagentConcurrencyControl";
import BrowserAccessNotice from "../BrowserAccessNotice";
import type { SetupRole } from "./AgentSetupPicker";
import type { useWorkerModels } from "./WorkerModelPicker";
import "./chat-settings-dialog.css";

const ClaudeSettings = lazy(() =>
  import("../ClaudeSettings").then((module) => ({
    default: module.ClaudeSettings,
  })),
);

type Props = {
  settingsOpen: boolean;
  accountModalOpen: boolean;
  setSettingsOpen: (value: boolean) => void;
  agent?: Agent | null;
  lead?: Agent | null;
  accounts: ReturnType<typeof useAccounts>;
  notify: (message: string) => unknown;
  updateAccountModalOpen: (opened: boolean) => void;
  agentModels: ReturnType<typeof useWorkerModels>;
  data: Snapshot;
  refresh: () => Promise<void>;
  workspaceId: string;
  accountKey: string;
  creating: boolean;
  newChat: (path?: string) => unknown;
  project: () => void;
  selectedAccount?: Account;
  setClaudeLoginKey: (key: string) => void;
};

export function ChatSettingsDialog(p: Props) {
  const mobile = useMediaQuery("(max-width: 48em)");
  const root = p.lead || p.agent;
  const [tab, setTab] = useState("models");
  useEffect(() => {
    if (p.settingsOpen) {
      setTab(p.agent?.source === "managed" ? "models" : "access");
      setRole("orchestrator");
    }
  }, [p.settingsOpen, p.agent?.id, p.agent?.source]);
  const [role, setRole] = useState<SetupRole>("orchestrator");
  const [claudePermission, setClaudePermission] = useState<string | null>(null);
  const roles: { value: SetupRole; label: string }[] = [
    { value: "orchestrator", label: p.agent?.isLead ? "Lead" : "Agent" },
    ...(root?.isLead
      ? [
          { value: "worker" as const, label: "Workers" },
          { value: "review" as const, label: "Review" },
        ]
      : []),
  ];
  const modelsAgent = role === "orchestrator" ? p.agent : root;
  return (
    <Modal
      opened={p.settingsOpen}
      onClose={() => {
        if (!p.accountModalOpen) p.setSettingsOpen(false);
      }}
      closeOnEscape={!p.accountModalOpen}
      closeOnClickOutside={!p.accountModalOpen}
      title="Chat settings"
      size={modalSizes.settings}
      fullScreen={mobile}
      className="execution-settings-modal"
    >
      <Tabs
        keepMountedMode="display-none"
        value={tab}
        onChange={(value) => {
          if (value) setTab(value);
        }}
        className="chat-execution-settings"
      >
        <Tabs.List grow aria-label="Chat settings sections">
          <Tabs.Tab value="models" disabled={p.agent?.source !== "managed"}>
            Models
          </Tabs.Tab>
          <Tabs.Tab value="team" disabled={root?.source !== "managed"}>
            Team{" "}
            <span className="settings-tab-summary">{root?.concurrency}</span>
          </Tabs.Tab>
          <Tabs.Tab value="access">
            Access{" "}
            <span className="settings-tab-summary">
              {p.agent?.provider === "claude"
                ? (
                    {
                      default: "Ask",
                      acceptEdits: "Edits",
                      auto: "Automatic",
                      plan: "Plan",
                      bypassPermissions: "Full",
                    } as Record<string, string>
                  )[claudePermission || ""] || "…"
                : p.agent?.yoloMode
                  ? "Full"
                  : "Ask"}
            </span>
          </Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="models" pt="sm">
          {modelsAgent?.source === "managed" && (
            <>
              {tab === "models" && (
                <SegmentedControl
                  fullWidth
                  size="xs"
                  aria-label="Agent role"
                  value={role}
                  onChange={(value) => setRole(value as SetupRole)}
                  className="settings-role-switch"
                  data={roles}
                />
              )}
              <UnifiedAgentSettings
                key={role}
                state={p.accounts}
                notify={p.notify}
                settingsRow
                presentation="panel"
                initialRole={role}
                onAccountModalOpenChange={p.updateAccountModalOpen}
                agent={modelsAgent}
                catalog={p.agentModels}
                team={p.data.runtime?.agents || []}
                refresh={p.refresh}
              />
            </>
          )}
        </Tabs.Panel>
        <Tabs.Panel value="team" pt="sm">
          <Stack gap="md">
            {root?.source === "managed" && (
              <section aria-label="Team">
                <div className="execution-team-fields">
                  <SubagentConcurrencyControl
                    lead={root}
                    stateDir={p.data.stateDir}
                    workspaceId={p.workspaceId}
                    refresh={p.refresh}
                  />
                  <ExecutionSettings
                    teamOnly
                    settingsRow
                    initialRole="worker"
                    agent={root}
                    catalog={p.agentModels}
                    accounts={p.accounts.data.accounts}
                    refresh={p.refresh}
                  />
                </div>
              </section>
            )}
            {root?.isLead && (
              <AccountExclusionsControl
                lead={root}
                accounts={p.accounts.data}
                refresh={p.refresh}
                onError={p.notify}
              />
            )}
          </Stack>
        </Tabs.Panel>
        <Tabs.Panel value="access" pt="sm">
          <Stack gap="md">
            <section aria-label="Access">
              <Stack gap="sm" id="chat-settings-permissions">
                {p.agent?.source === "managed" &&
                  p.agent.provider !== "claude" && (
                    <ExecutionSettings
                      permissionsOnly
                      settingsRow
                      agent={p.agent}
                      catalog={p.agentModels}
                      refresh={p.refresh}
                    />
                  )}
                {p.agent?.provider === "claude" && (
                  <Suspense fallback={null}>
                    <ClaudeSettings
                      key={p.agent.id}
                      onPermissionModeChange={setClaudePermission}
                      agent={p.agent}
                      account={p.selectedAccount}
                      onSignIn={(key) => {
                        p.setSettingsOpen(false);
                        p.setClaudeLoginKey(key);
                      }}
                    />
                  </Suspense>
                )}
                <BrowserAccessNotice
                  compact
                  accountKey={p.accountKey}
                  active={
                    p.settingsOpen && (p.agent || p.lead)?.provider !== "claude"
                  }
                />
              </Stack>
            </section>
            <Group
              justify="flex-end"
              gap="xs"
              className="execution-dialog-actions"
            >
              {p.agent?.cwd && (
                <Button
                  variant="default"
                  disabled={p.creating}
                  onClick={() => {
                    p.setSettingsOpen(false);
                    void p.newChat(p.agent?.cwd || undefined);
                  }}
                >
                  New chat in this project
                </Button>
              )}
              {p.agent?.cwd && (
                <Button
                  variant="subtle"
                  onClick={() => {
                    p.setSettingsOpen(false);
                    p.project();
                  }}
                >
                  Choose project folder
                </Button>
              )}
            </Group>
          </Stack>
        </Tabs.Panel>
      </Tabs>
    </Modal>
  );
}
