import { lazy, Suspense, type RefObject } from "react";
import { Button, Modal, Tooltip } from "@mantine/core";
import { Folder, Plus } from "lucide-react";
import { busy, type Agent, type Snapshot } from "../../types";
import { modalSizes } from "../../theme";
import { menuActions } from "../../nativeCommands";
import { nativeThreadError } from "../../nativeErrors";
import { SettingsRow } from "../ui/primitives";
import { UnifiedAgentSettings } from "./UnifiedAgentSettings";
import SubagentConcurrencyControl from "./SubagentConcurrencyControl";
import BrowserAccessNotice from "../BrowserAccessNotice";
import type { Account, useAccounts } from "../Accounts";
import type { useWorkerModels } from "./WorkerModelPicker";
const ClaudeSettings = lazy(() =>
  import("../ClaudeSettings").then((module) => ({
    default: module.ClaudeSettings,
  })),
);

type Props = {
  settingsOpen: boolean;
  accountModalOpen: boolean;
  mainSettingsOpen: boolean;
  workerSettingsOpen: boolean;
  reviewSettingsOpen: boolean;
  accountModalOpenRef: RefObject<boolean>;
  setSettingsOpen: (value: boolean) => void;
  agent?: Agent | null;
  lead?: Agent | null;
  accounts: ReturnType<typeof useAccounts>;
  notify: (message: string) => unknown;
  setMainSettingsOpen: (value: boolean) => void;
  updateAccountModalOpen: (value: boolean) => void;
  agentModels: ReturnType<typeof useWorkerModels>;
  data: Snapshot;
  refresh: () => Promise<void>;
  setWorkerSettingsOpen: (value: boolean) => void;
  setReviewSettingsOpen: (value: boolean) => void;
  setAccountModalOpen: (value: boolean) => void;
  workspaceId: string;
  accountKey: string;
  creating: boolean;
  newChat: (path?: string) => unknown;
  run: (action: () => Promise<unknown>) => unknown;
  submitNativeAction: (
    agent: Agent,
    action: "compact" | "review",
  ) => Promise<unknown>;
  project: () => void;
  selectedAccount?: Account;
  setClaudeLoginKey: (key: string) => void;
};

export function ChatSettingsDialog({
  settingsOpen,
  accountModalOpen,
  mainSettingsOpen,
  workerSettingsOpen,
  reviewSettingsOpen,
  accountModalOpenRef,
  setSettingsOpen,
  agent,
  lead,
  accounts,
  notify,
  setMainSettingsOpen,
  updateAccountModalOpen,
  agentModels,
  data,
  refresh,
  setWorkerSettingsOpen,
  setReviewSettingsOpen,
  setAccountModalOpen,
  workspaceId,
  accountKey,
  creating,
  newChat,
  run,
  submitNativeAction,
  project,
  selectedAccount,
  setClaudeLoginKey,
}: Props) {
  return (
    <Modal
      opened={settingsOpen}
      closeOnEscape={
        !accountModalOpen &&
        !mainSettingsOpen &&
        !workerSettingsOpen &&
        !reviewSettingsOpen
      }
      closeOnClickOutside={
        !accountModalOpen &&
        !mainSettingsOpen &&
        !workerSettingsOpen &&
        !reviewSettingsOpen
      }
      onClose={() => {
        if (accountModalOpenRef.current) return;
        setSettingsOpen(false);
      }}
      title="Chat settings"
      size={modalSizes.settings}
    >
      <div className="chat-settings-panel chat-settings-rows">
        {agent?.source === "managed" && (
          <>
            <SettingsRow label="Model">
              <UnifiedAgentSettings
                key={"execution:" + agent.id}
                state={accounts}
                notify={notify}
                settingsRow
                permissionsTargetId="chat-settings-permissions"
                extrasTargetId="chat-settings-modes"
                onOpenChange={setMainSettingsOpen}
                onAccountModalOpenChange={updateAccountModalOpen}
                showAccountExclusions
                agent={agent}
                catalog={agentModels}
                team={data?.runtime?.agents || []}
                refresh={refresh}
              />
            </SettingsRow>
            {lead?.isLead &&
              (["worker", "review"] as const).map((role) => (
                <SettingsRow
                  key={role}
                  label={role === "worker" ? "Workers" : "Review"}
                >
                  <UnifiedAgentSettings
                    state={accounts}
                    notify={notify}
                    settingsRow
                    initialRole={role}
                    onOpenChange={
                      role === "worker"
                        ? setWorkerSettingsOpen
                        : setReviewSettingsOpen
                    }
                    onAccountModalOpenChange={setAccountModalOpen}
                    agent={lead}
                    catalog={agentModels}
                    team={data?.runtime?.agents || []}
                    refresh={refresh}
                  />
                </SettingsRow>
              ))}
          </>
        )}
        {lead?.source === "managed" && (
          <SettingsRow label="Parallel">
            <SubagentConcurrencyControl
              compact
              lead={lead}
              stateDir={data.stateDir}
              workspaceId={workspaceId}
              refresh={refresh}
            />
          </SettingsRow>
        )}
        <div id="chat-settings-permissions" />
        <BrowserAccessNotice
          compact
          accountKey={accountKey}
          active={settingsOpen && (agent || lead)?.provider !== "claude"}
        />
        <div className="chat-settings-footer">
          {agent?.cwd && (
            <Button
              variant="default"
              className="settings-new-chat"
              leftSection={<Plus size={14} />}
              aria-label="New chat in this project"
              disabled={creating}
              onClick={() => {
                setSettingsOpen(false);
                void newChat(agent.cwd ?? undefined);
              }}
            >
              New chat
            </Button>
          )}
          {agent?.source === "managed" &&
            (["compact", "review"] as const)
              .filter((action) =>
                menuActions(agent.provider ?? undefined).includes(action),
              )
              .map((action) => {
                const disabled =
                  busy.has(agent.status ?? "") ||
                  !!agent.inFlight ||
                  !!nativeThreadError(agent) ||
                  !agent.threadId;
                return (
                  <Tooltip
                    key={action}
                    disabled={!disabled}
                    label={
                      !agent.threadId
                        ? "Available after the chat starts."
                        : "Wait until the chat is ready."
                    }
                  >
                    <span>
                      <Button
                        variant="default"
                        disabled={disabled}
                        onClick={() => {
                          setSettingsOpen(false);
                          void run(() => submitNativeAction(agent, action));
                        }}
                      >
                        {action === "compact" ? "Compact" : "Review"}
                      </Button>
                    </span>
                  </Tooltip>
                );
              })}
        </div>
        <details className="chat-settings-more">
          <summary>More</summary>
          <div id="chat-settings-modes" />
          {agent?.cwd && (
            <Button
              leftSection={<Folder size={14} />}
              aria-label="Choose project folder"
              onClick={() => {
                setSettingsOpen(false);
                project();
              }}
            >
              Folder
            </Button>
          )}
          {agent?.provider === "claude" && (
            <Suspense fallback={null}>
              <ClaudeSettings
                agent={agent}
                account={selectedAccount}
                permissionsTargetId="chat-settings-permissions"
                onSignIn={(key) => {
                  setSettingsOpen(false);
                  setClaudeLoginKey(key);
                }}
              />
            </Suspense>
          )}
        </details>
      </div>
    </Modal>
  );
}
