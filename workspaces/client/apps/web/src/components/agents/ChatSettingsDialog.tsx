import { lazy, Suspense, useEffect, useState } from "react";
import { Button, Group, Modal, Stack, Tabs } from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import type { Agent, Snapshot } from "../../types";
import { modalSizes } from "../../theme";
import type { Account, useAccounts } from "../Accounts";
import { UnifiedAgentSettings } from "./UnifiedAgentSettings";
import { DelegationSettings, ExecutionPermissions } from "./ExecutionSettings";
import { AccountExclusionsControl } from "./AccountExclusionsControl";
import SubagentConcurrencyControl from "./SubagentConcurrencyControl";
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
      setTab(p.agent?.source === "managed" ? "models" : "team");
    }
  }, [p.settingsOpen, p.agent?.id, p.agent?.source]);
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
          <Tabs.Tab value="team">
            Team{" "}
            <span className="settings-tab-summary">{root?.concurrency}</span>
          </Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="models" pt="sm">
          {p.agent?.source === "managed" && (
            <UnifiedAgentSettings
              key={p.agent.id}
              state={p.accounts}
              notify={p.notify}
              settingsRow
              presentation="panel"
              onAccountModalOpenChange={p.updateAccountModalOpen}
              agent={p.agent}
              lead={root?.isLead ? root : undefined}
              catalog={p.agentModels}
              team={p.data.runtime?.agents || []}
              refresh={p.refresh}
            />
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
                  <DelegationSettings
                    settingsRow
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
            <section aria-label="Permissions">
              <Stack gap="sm" id="chat-settings-permissions">
                {p.agent?.source === "managed" &&
                  p.agent.provider !== "claude" && (
                    <ExecutionPermissions
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
                      agent={p.agent}
                      account={p.selectedAccount}
                      onSignIn={(key) => {
                        p.setSettingsOpen(false);
                        p.setClaudeLoginKey(key);
                      }}
                    />
                  </Suspense>
                )}
              </Stack>
            </section>
          </Stack>
        </Tabs.Panel>
      </Tabs>
      <Group justify="flex-end" gap="xs" className="execution-dialog-actions">
        {p.agent?.cwd && (
          <Button
            size="xs"
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
            size="xs"
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
    </Modal>
  );
}
