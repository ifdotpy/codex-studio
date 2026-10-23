import { lazy, Suspense } from "react";
import { Button, Group, Modal, Stack } from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import type { Agent, Snapshot } from "../../types";
import { modalSizes } from "../../theme";
import type { Account, useAccounts } from "../Accounts";
import type { useWorkerModels } from "./WorkerModelPicker";
import { UnifiedAgentSettings } from "./UnifiedAgentSettings";
import { ExecutionSettings } from "./ExecutionSettings";
import { AccountExclusionsControl } from "./AccountExclusionsControl";
import SubagentConcurrencyControl from "./SubagentConcurrencyControl";
import BrowserAccessNotice from "../BrowserAccessNotice";
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
      <Stack gap="md" className="chat-execution-settings">
        {p.agent?.source === "managed" && (
          <section aria-label="Agents">
            <Stack gap="0">
              <UnifiedAgentSettings
                state={p.accounts}
                notify={p.notify}
                settingsRow
                presentation="disclosure"
                onAccountModalOpenChange={p.updateAccountModalOpen}
                agent={p.agent}
                catalog={p.agentModels}
                team={p.data.runtime?.agents || []}
                refresh={p.refresh}
              />
              {root?.isLead &&
                (["worker", "review"] as const).map((role) => (
                  <UnifiedAgentSettings
                    key={role}
                    state={p.accounts}
                    notify={p.notify}
                    settingsRow
                    presentation="disclosure"
                    initialRole={role}
                    onAccountModalOpenChange={p.updateAccountModalOpen}
                    agent={root}
                    catalog={p.agentModels}
                    team={p.data.runtime?.agents || []}
                    refresh={p.refresh}
                  />
                ))}
            </Stack>
          </section>
        )}
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
        <section aria-label="Access">
          <Stack gap="sm" id="chat-settings-permissions">
            {p.agent?.source === "managed" && p.agent.provider !== "claude" && (
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
        <Group justify="flex-end" gap="xs" className="execution-dialog-actions">
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
    </Modal>
  );
}
